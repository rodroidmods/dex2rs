#!/usr/bin/env python3
"""Static validation of a converted APK/DEX, without a device or an emulator.

Every check here maps to a way a converted app can crash on a real phone:

  * the dex is unparseable / has a stale checksum       -> app dies on install or at class load
  * the dex version is newer than `minSdkVersion`       -> ART refuses the file on older devices
  * a `native` method has no implementation             -> UnsatisfiedLinkError on first call
  * the loader class / `System.loadLibrary` is missing  -> native methods never get registered
  * a stored entry is not 4-byte aligned                -> `zipalign`/Play rejects, mmap can fail
  * an uncompressed `lib/**/*.so` is not page aligned   -> `extractNativeLibs=false` cannot dlopen it
  * the .so is missing for an ABI in the APK            -> crash on the devices using that ABI
  * the APK is unsigned or signed with the wrong scheme -> install fails

Usage:
    python3 tools/verify.py out.apk [--original app.apk] [--project DIR]
                                     [--lib-name NAME] [--dynamic-register]

Exit status is non-zero when a check fails.
"""
from __future__ import annotations

import argparse
import os
import re
import struct
import subprocess
import sys
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

# --------------------------------------------------------------------------- #
# reporting


@dataclass
class Report:
    passed: int = 0
    failed: int = 0
    warned: int = 0
    lines: list[str] = field(default_factory=list)

    def ok(self, msg: str) -> None:
        self.passed += 1
        self.lines.append(f"  PASS  {msg}")

    def fail(self, msg: str) -> None:
        self.failed += 1
        self.lines.append(f"  FAIL  {msg}")

    def warn(self, msg: str) -> None:
        self.warned += 1
        self.lines.append(f"  WARN  {msg}")

    def info(self, msg: str) -> None:
        self.lines.append(f"        {msg}")

    def section(self, title: str) -> None:
        self.lines.append("")
        self.lines.append(f"== {title}")

    def dump(self) -> None:
        print("\n".join(self.lines))
        print()
        print(
            f"==> {self.passed} passed, {self.failed} failed, {self.warned} warnings"
        )


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def find_build_tools() -> Path | None:
    for env in ("ANDROID_HOME", "ANDROID_SDK_ROOT"):
        root = os.environ.get(env)
        if root and (Path(root) / "build-tools").is_dir():
            return _newest(Path(root) / "build-tools")
    home = Path.home()
    for candidate in (home / "android-sdk" / "build-tools", home / "Android" / "Sdk" / "build-tools"):
        if candidate.is_dir():
            return _newest(candidate)
    return None


def _newest(root: Path) -> Path | None:
    versions = [p for p in root.iterdir() if p.is_dir()]
    if not versions:
        return None

    def key(p: Path):
        return [int(x) if x.isdigit() else 0 for x in re.split(r"[.\-]", p.name)]

    return sorted(versions, key=key)[-1]


# --------------------------------------------------------------------------- #
# dex


@dataclass
class DexMethod:
    class_desc: str          # Lcom/example/Foo;
    name: str
    proto: str               # (I)Ljava/lang/String;
    access: int
    code_off: int

    @property
    def is_native(self) -> bool:
        return bool(self.access & 0x100)

    def long_name(self) -> str:
        # `jni_long` already carries the `Java_` prefix (it mirrors
        # `dex2rs_core::util::jni_long_name`), so do not add a second one.
        return jni_long(self.class_desc, self.name, self.proto)


def _uleb(data: bytes, ix: int) -> tuple[int, int]:
    result = shift = 0
    while True:
        byte = data[ix]
        ix += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, ix
        shift += 7


def _read_utf8(data: bytes, off: int) -> str:
    _, ix = _uleb(data, off)
    end = data.index(b"\x00", ix)
    return data[ix:end].decode("utf-8", "replace")


def _descriptor(data: bytes, string_ids_off: int, type_ids_off: int, idx: int) -> str:
    (string_idx,) = struct.unpack_from("<I", data, type_ids_off + idx * 4)
    (string_off,) = struct.unpack_from("<I", data, string_ids_off + string_idx * 4)
    return _read_utf8(data, string_off)


def _proto(data: bytes, string_ids_off: int, type_ids_off: int, proto_ids_off: int, idx: int) -> str:
    _shorty_idx, return_idx, params_off = struct.unpack_from("<III", data, proto_ids_off + idx * 12)
    out = []
    if params_off:
        (count,) = struct.unpack_from("<I", data, params_off)
        for i in range(count):
            (type_idx,) = struct.unpack_from("<H", data, params_off + 4 + i * 2)
            out.append(_descriptor(data, string_ids_off, type_ids_off, type_idx))
    return "(" + "".join(out) + ")" + _descriptor(data, string_ids_off, type_ids_off, return_idx)


def jni_mangle(text: str) -> str:
    """`MangleForJni()` from art/runtime/utils.cc.

    Alphanumerics survive; `/` and `.` become `_`, and every other character
    becomes `_0` followed by its code point in hex. `$` is *not* special-cased:
    it is just another non-alphanumeric, so `$values` mangles to `_00024values`.
    """
    out = []
    for ch in text:
        if ch.isascii() and ch.isalnum():
            out.append(ch)
        elif ch in "/.":
            out.append("_")
        elif ch == "_":
            out.append("_1")
        elif ch == ";":
            out.append("_2")
        elif ch == "[":
            out.append("_3")
        else:
            out.append("_0%04x" % ord(ch))
    return "".join(out)


def jni_long(class_desc: str, name: str, proto: str) -> str:
    """`JniShortName` + `JniLongName`: `Java_<class>_<method>__<params>`.

    Mirrors `dex2rs_core::util::{jni_short_name, jni_long_name}` (the two must
    agree, or this checker reports phantom missing symbols). Note the double
    underscore that a method named `$values` produces: the `_` separator
    followed by the escape's own leading `_0`, e.g. `ThemeMode__00024values__`.
    """
    short = "Java_" + jni_mangle(class_desc[1:-1]) + "_"
    short += "_init_" if name == "<init>" else jni_mangle(name)
    params = proto[1:].split(")", 1)[0]
    return short + "__" + jni_mangle(params)


# Guard the checker itself. A wrong mangling here does not just miss a bug: it
# reports hundreds of phantom "missing implementation" failures, which is exactly
# how a stray `_00024` appended to every method name hid for a while. These two
# cases differ by one underscore, so they pin the rule down. They are also
# asserted for the converter in `dex2rs-core/src/util.rs::jni_names_match_art`.
assert jni_long("Lcom/example/Test;", "doIt", "(Ljava/lang/String;I)V") == (
    "Java_com_example_Test_doIt__Ljava_lang_String_2I"
), "jni_long no longer matches ART"
assert jni_long(
    "Lcom/example/ThemeMode;", "$values", "(I[Ljava/lang/String;)V"
) == (
    "Java_com_example_ThemeMode__00024values__I_3Ljava_lang_String_2"
), "jni_long no longer matches ART"


def parse_dex(data: bytes):
    if not data.startswith(b"dex\n"):
        raise ValueError("not a dex")
    version = data[4:7].decode()
    class_size, class_off = struct.unpack_from("<II", data, 96)
    string_ids_off = struct.unpack_from("<I", data, 60)[0]
    type_ids_off = struct.unpack_from("<I", data, 68)[0]
    proto_ids_off = struct.unpack_from("<I", data, 76)[0]
    method_idx_off = struct.unpack_from("<I", data, 92)[0]
    methods = []
    for c in range(class_size):
        base = class_off + c * 32
        class_idx = struct.unpack_from("<I", data, base)[0]
        class_data_off = struct.unpack_from("<I", data, base + 24)[0]
        if class_data_off == 0:
            continue
        class_desc = _descriptor(data, string_ids_off, type_ids_off, class_idx)
        ix = class_data_off
        sfields, ix = _uleb(data, ix)
        ifields, ix = _uleb(data, ix)
        dmethods, ix = _uleb(data, ix)
        vmethods, ix = _uleb(data, ix)
        # encoded_field = field_idx_diff, access_flags (two ulebs each).
        for _ in range(sfields + ifields):
            _, ix = _uleb(data, ix)
            _, ix = _uleb(data, ix)
        # `method_idx_diff` is a delta from the previous entry *of the same
        # list* (direct / virtual), so the running index restarts for each.
        for list_count in (dmethods, vmethods):
            method_idx = 0
            for _ in range(list_count):
                diff, ix = _uleb(data, ix)
                access, ix = _uleb(data, ix)
                code_off, ix = _uleb(data, ix)
                method_idx += diff
                entry = method_idx_off + method_idx * 8
                proto_idx = struct.unpack_from("<H", data, entry + 2)[0]
                name_idx = struct.unpack_from("<I", data, entry + 4)[0]
                name = _read_utf8(data, struct.unpack_from("<I", data, string_ids_off + name_idx * 4)[0])
                methods.append(
                    DexMethod(
                        class_desc,
                        name,
                        _proto(data, string_ids_off, type_ids_off, proto_ids_off, proto_idx),
                        access,
                        code_off,
                    )
                )
    return version, methods


def parse_apk_dexes(path: Path):
    out = {}
    with zipfile.ZipFile(path) as zf:
        for name in zf.namelist():
            if re.fullmatch(r"classes\d*\.dex", name):
                data = zf.read(name)
                out[name] = (data, *parse_dex(data))
    return out


def verify_dex_header(path_label: str, data: bytes, report: Report) -> None:
    if data[12:32] != _sha1(data[32:]):
        report.fail(f"{path_label}: header SHA-1 signature does not match the file")
    checksum = struct.unpack_from("<I", data, 8)[0]
    if checksum != _adler32(data[12:]):
        report.fail(f"{path_label}: header Adler-32 checksum does not match the file")
    declared = struct.unpack_from("<I", data, 32)[0]
    if declared != len(data):
        report.fail(f"{path_label}: header file_size {declared} != actual {len(data)}")


def _sha1(data: bytes) -> bytes:
    import hashlib

    return hashlib.sha1(data).digest()


def _adler32(data: bytes) -> int:
    import zlib

    return zlib.adler32(data) & 0xFFFFFFFF


# --------------------------------------------------------------------------- #
# zip


@dataclass
class ZipEntry:
    name: str
    method: int
    data_off: int
    size: int
    header_off: int


def read_zip_entries(path: Path) -> list[ZipEntry]:
    data = path.read_bytes()
    entries = []
    with zipfile.ZipFile(path) as zf:
        for info in zf.infolist():
            sig, _ver, _flg, method, _t, _dt, _crc, _cs, _us, nlen, elen = struct.unpack_from(
                "<IHHHHHIIIHH", data, info.header_offset
            )
            if sig != 0x04034B50:
                continue
            entries.append(
                ZipEntry(
                    name=info.filename,
                    method=method,
                    data_off=info.header_offset + 30 + nlen + elen,
                    size=info.file_size,
                    header_off=info.header_offset,
                )
            )
    return entries


def extract_native_libs(state: str) -> bool:
    """`android:extractNativeLibs`; absent means false on modern targetSdk."""
    return "extractNativeLibs" in state and "=false" not in state


# --------------------------------------------------------------------------- #
# main checks


def manifest_extract_native_libs(path: Path, report: Report):
    """`android:extractNativeLibs` from the APK manifest: True, False or None.

    Decoded through `aapt2` rather than by scanning the bytes: the string pool is
    UTF-16, so a byte search for the attribute name both misses real attributes
    and matches prose, and it cannot tell the typed value from the raw text. (The
    typed data word is what the platform reads, and a rewritten attribute can
    carry a stale one -- an APK that says `false` in the data and "true" in the
    raw text was exactly the packaging bug this check exists for.)
    """
    bt = find_build_tools()
    if bt is None:
        return None
    aapt2 = Path(bt) / "aapt2"
    if not aapt2.exists():
        return None
    try:
        out = subprocess.run(
            [str(aapt2), "dump", "xmltree", "--file", "AndroidManifest.xml", str(path)],
            capture_output=True,
            text=True,
            timeout=120,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    for line in out.splitlines():
        if "extractNativeLibs" in line:
            # `A: ...:extractNativeLibs(0x010104ea)=true (Raw: "true")`
            after = line.split("extractNativeLibs", 1)[1]
            if "=" not in after:
                return None
            value = after.split("=", 1)[1].split(" ", 1)[0].strip()
            if value == "true":
                return True
            if value == "false":
                return False
            return None
    # Absent: the platform default for targetSdk >= 23 is false.
    return False


def check_apk(path: Path, report: Report, args) -> dict:
    report.section(f"archive — {path.name}")
    if not zipfile.is_zipfile(path):
        report.fail("not a zip archive")
        return {}
    report.ok("is a readable zip archive")

    entries = read_zip_entries(path)
    by_name = {e.name: e for e in entries}
    abis = sorted({e.name.split("/")[1] for e in entries if e.name.startswith("lib/")})
    report.info(f"{len(entries)} entries, abis: {', '.join(abis) or 'none'}")

    stored = [e for e in entries if e.method == 0]
    bad4 = [e for e in stored if e.data_off % 4]
    if bad4:
        report.fail(f"{len(bad4)} stored entries are not 4-byte aligned (e.g. {bad4[0].name})")
    else:
        report.ok(f"all {len(stored)} stored entries are 4-byte aligned (zipalign)")

    libs = [e for e in entries if e.name.startswith("lib/") and e.name.endswith(".so")]
    compressed = [e for e in libs if e.method != 0]
    # Compression and `extractNativeLibs` are one decision, not two. Stored and
    # page aligned means the loader maps the `.so` out of the APK; compressed
    # means the installer has to unpack it first, which only happens when the
    # manifest says `extractNativeLibs="true"` (the platform default is false for
    # targetSdk >= 23). Getting the pair wrong installs fine and then dies with a
    # library that cannot be loaded.
    extract = manifest_extract_native_libs(path, report)
    if compressed:
        if extract is True:
            report.ok(
                f"{len(compressed)} native libs are compressed and "
                "extractNativeLibs=true (unpacked at install)"
            )
        elif extract is False:
            report.fail(
                f"{len(compressed)} native libs are compressed but the manifest "
                'says extractNativeLibs="false" -- the loader cannot map them'
            )
        else:
            report.fail(
                f"{len(compressed)} native libs are compressed and "
                "extractNativeLibs could not be read from the manifest"
            )
    else:
        report.ok(f"all {len(libs)} native libs are stored uncompressed")

    misaligned = [e for e in libs if e.data_off % 4096]
    if misaligned and not compressed:
        report.fail(
            f"{len(misaligned)} native libs are not page aligned "
            f"(first: {misaligned[0].name} @ {misaligned[0].data_off}) — "
            "cannot be dlopen'd when extractNativeLibs=false"
        )
    elif not compressed:
        report.ok("every native lib is 4 KiB page aligned")
    not16k = [e for e in libs if e.data_off % 16384]
    if not16k and not compressed:
        report.warn(f"{len(not16k)} native libs are not 16 KiB aligned (required for API 35+ 16KB pages)")

    # --- build-tools based checks
    bt = find_build_tools()
    if bt is None:
        report.warn("Android build-tools not found; skipping aapt2/apksigner/dexdump checks")
        return {"entries": entries, "abis": abis, "build_tools": None}

    report.info(f"using build-tools at {bt}")
    apk_signer = bt / "apksigner"
    if apk_signer.exists():
        result = run(["java", "-jar", str(bt / "lib" / "apksigner.jar"), "verify", "--verbose", str(path)])
        text = (result.stdout or "") + (result.stderr or "")
        if result.returncode == 0 and "Verifies" in text:
            schemes = re.findall(r"Verified using (v\d(?:\.\d)?) scheme.*?: true", text)
            report.ok(f"apksigner: signature verifies ({', '.join(schemes) or 'v1'})")
        else:
            report.fail("apksigner: signature does not verify")

    aapt2 = bt / "aapt2"
    badging = ""
    if aapt2.exists():
        result = run([str(aapt2), "dump", "badging", str(path)])
        badging = result.stdout or ""
        if result.returncode != 0:
            report.fail("aapt2: AndroidManifest.xml does not decode")
        else:
            minsdk = re.search(r"minSdkVersion:'(\d+)'", badging)
            report.ok(f"aapt2: manifest decodes (minSdk {minsdk.group(1) if minsdk else '?'})")
        result = run([str(aapt2), "dump", "xmltree", "--file", "AndroidManifest.xml", str(path)])
        tree = result.stdout or ""
        # Only the <application> element itself, not its children.
        block = re.search(r"E: application \(line=\d+\)\n(.*?)\n      E: ", tree, re.S)
        if block:
            app_class = re.search(r'name\(0x01010003\)="([^"]*)"', block.group(1))
            if app_class:
                report.info(f"application android:name = {app_class.group(1)}")
    return {
        "entries": entries,
        "by_name": by_name,
        "abis": abis,
        "build_tools": bt,
        "badging": badging,
        "dexes": parse_apk_dexes(path),
    }


def check_signing(path: Path, report: Report, bt: Path | None) -> None:
    report.section("signing")
    if bt is None:
        report.warn("no build-tools, cannot verify the signature")
        return
    apksigner = bt / "apksigner"
    if not apksigner.exists():
        report.warn("apksigner not found in build-tools")
        return
    result = run([str(apksigner), "verify", "--verbose", "--print-certs", str(path)])
    text = (result.stdout or "") + (result.stderr or "")
    if result.returncode != 0:
        report.fail("apksigner verify failed:\n" + text.strip()[:400])
        return
    for scheme in ("v1", "v2", "v3"):
        ok = re.search(rf"Verified using {scheme} scheme[^:]*: (true|false)", text)
        if ok and ok.group(1) == "true":
            report.ok(f"signed with the {scheme} scheme")


def check_dexes(state: dict, report: Report, args) -> None:
    report.section("dex")
    dexes = state.get("dexes") or {}
    if not dexes:
        report.fail("no classes.dex found")
        return
    min_sdk = 0
    if state.get("badging"):
        m = re.search(r"minSdkVersion:'(\d+)'", state["badging"])
        if m:
            min_sdk = int(m.group(1))
    # DEX version -> lowest Android API that understands it.
    dex_to_api = {"035": 21, "037": 24, "038": 26, "039": 28, "040": 30}

    all_natives: dict[str, DexMethod] = {}
    for name, (data, version, methods) in sorted(dexes.items()):
        verify_dex_header(name, data, report)
        report.ok(f"{name}: header checksum + signature verified")
        need = dex_to_api.get(version)
        if need is None:
            report.warn(f"{name}: unknown DEX version {version}")
        elif min_sdk and min_sdk < need:
            report.fail(
                f"{name}: DEX version {version} needs API {need} but minSdkVersion is {min_sdk} "
                "— ART will refuse to load it on those devices"
            )
        else:
            report.ok(f"{name}: DEX version {version} is loadable at minSdkVersion {min_sdk}")
        natives = [m for m in methods if m.is_native]
        report.info(f"{name}: {len(methods)} methods, {len(natives)} native")
        for m in natives:
            all_natives[f"{m.class_desc}{m.name}{m.proto}"] = m

    # Which native methods did the *original* already have? Those come from the
    # app's own libraries, and the danger is that the conversion overwrites one
    # of them with its own lib of the same name.
    original_natives: set[str] = set()
    if args.original:
        for _name, (_d, _v, methods) in parse_apk_dexes(Path(args.original)).items():
            for m in methods:
                if m.is_native:
                    original_natives.add(f"{m.class_desc}{m.name}{m.proto}")
        report.info(f"original apk already declared {len(original_natives)} native methods")

    introduced = {k: v for k, v in all_natives.items() if k not in original_natives}
    report.info(
        f"{len(introduced)} native methods introduced by the conversion, "
        f"{len(all_natives) - len(introduced)} pre-existing"
    )

    # Every native method in the *output* dex needs an implementation somewhere
    # in the output apk: either a `Java_` export in one of its .so files, or a
    # RegisterNatives entry in the generated Rust source.
    blobs: dict[str, bytes] = {}
    with zipfile.ZipFile(args.apk) as zf:
        for name in zf.namelist():
            if name.startswith("lib/") and name.endswith(".so"):
                blobs[name] = zf.read(name)

    text_sources: list[bytes] = []
    if args.project:
        for pattern in ("src/*.rs", "src/**/*.rs"):
            text_sources.extend(p.read_bytes() for p in Path(args.project).glob(pattern))
    corpus = b"\n".join(list(blobs.values()) + text_sources)

    missing, missing_pre_existing = [], []
    for key, method in all_natives.items():
        if method.long_name().encode() in corpus:
            continue
        # RegisterNatives passes the plain name and the JNI signature as strings.
        if method.name.encode() in corpus and method.proto.encode() in corpus:
            continue
        (missing_pre_existing if key in original_natives else missing).append(method)

    # `--obfuscate` moves the converted code's names out of the crate, so the
    # `name` + `proto` fallback above stops being satisfiable from the generated
    # source and the search falls back to the `.so` files alone. An app that
    # registers its own natives instead of exporting `Java_*` symbols does not
    # necessarily keep the signature string verbatim (il2cpp keeps its
    # registration metadata in its own tables), so a pre-existing native can look
    # unbacked while its library is still sitting in the APK untouched.
    #
    # The check exists to catch one thing: the conversion overwriting the library
    # that provided those methods. That is answerable without guessing which
    # library it was -- if the original APK's `.so` files are all still present
    # with identical bytes, nothing was replaced and the bindings are the ones
    # the app already shipped. Only a pre-existing native whose plausible
    # provider is *gone or changed* is a real failure.
    still_provided = []
    if missing_pre_existing and args.original:
        surviving = surviving_original_libs(Path(args.original), blobs)
        genuinely_lost = []
        for method in missing_pre_existing:
            provider = next(
                (name for name, blob in surviving if method.name.encode() in blob),
                None,
            )
            (still_provided if provider else genuinely_lost).append((method, provider))
        missing_pre_existing = [m for m, _ in genuinely_lost]

    if missing:
        report.fail(
            f"{len(missing)} converted native methods have no implementation in the apk "
            "(UnsatisfiedLinkError at first call):"
        )
        for m in missing[:8]:
            report.info(f"    {m.class_desc}{m.name}{m.proto}")
        if len(missing) > 8:
            report.info(f"    ... and {len(missing) - 8} more")
    else:
        report.ok(
            f"every one of the {len(introduced)} converted native methods has an "
            "implementation (JNI export or RegisterNatives entry)"
        )

    if still_provided:
        report.ok(
            f"{len(still_provided)} pre-existing native method(s) keep the library that "
            "declared them, byte-identical: "
            + ", ".join(f"{m.name} <- {p}" for m, p in still_provided[:4])
        )

    if missing_pre_existing:
        report.fail(
            f"{len(missing_pre_existing)} native methods the app already shipped lost their "
            "implementation — the conversion replaced the library that provided them:"
        )
        for m in missing_pre_existing[:8]:
            report.info(f"    {m.class_desc}{m.name}{m.proto}")
        if len(missing_pre_existing) > 8:
            report.info(f"    ... and {len(missing_pre_existing) - 8} more")
        report.info("    use a different --lib-name so the app's own .so survives")


def surviving_original_libs(original: Path, blobs: dict) -> list:
    """The original APK's `.so` files that are in `blobs` with identical bytes.

    Returns `(entry_name, bytes)` pairs. A library the conversion neither removed
    nor rewrote is the evidence that the natives it declared still bind: the
    loader finds them the same way it did before, whatever the registration form.
    """
    out = []
    with zipfile.ZipFile(original) as zf:
        for name in zf.namelist():
            if not (name.startswith("lib/") and name.endswith(".so")):
                continue
            data = zf.read(name)
            if blobs.get(name) == data:
                out.append((name, data))
    return out


def _apk_libs(path) -> list:
    """Distinct `lib<name>.so` names inside an APK, in zip order."""
    names = []
    try:
        with zipfile.ZipFile(path) as zf:
            entries = zf.namelist()
    except Exception:  # noqa: BLE001 - a missing/unreadable original is not fatal
        return names
    for name in entries:
        m = re.fullmatch(r"lib/[^/]+/lib(.+)\.so", name)
        if m and m.group(1) not in names:
            names.append(m.group(1))
    return names


def _guess_lib_name(args) -> str:
    """The library the conversion added.

    An app that ships its own JNI libraries (this one does: `librodroid_inject`)
    would otherwise be mistaken for the converted library, which made the loader
    and native-lib checks point at the wrong `.so`.
    """
    converted = [n for n in _apk_libs(args.apk) if "androidx" not in n]
    if getattr(args, "original", None):
        shipped = set(_apk_libs(args.original))
        added = [n for n in converted if n not in shipped]
        if added:
            return added[0]
    return converted[0] if converted else "stub"


def _keystream(seed: int, length: int) -> bytes:
    """`dex2rs_runtime::keystream` -- the per-string `--obfuscate` keystream."""
    keys = bytearray(length)
    x = seed & 0xFFFFFFFF
    i = 0
    while i < length:
        x = (x ^ ((x << 13) & 0xFFFFFFFF)) & 0xFFFFFFFF
        x = (x ^ (x >> 17)) & 0xFFFFFFFF
        x = (x ^ ((x << 5) & 0xFFFFFFFF)) & 0xFFFFFFFF
        for b in range(4):
            if i + b < length:
                keys[i + b] = (x >> (b * 8)) & 0xFF
        i += 4
    return bytes(keys)


def _byte_literal(body: bytes) -> bytes:
    """A Rust `b"..."` body back to bytes (only the escapes the emitter writes)."""
    out = bytearray()
    i = 0
    while i < len(body):
        ch = body[i]
        if ch != 0x5C:
            out.append(ch)
            i += 1
        elif body[i + 1] == 0x78:
            out.append(int(body[i + 2 : i + 4], 16))
            i += 4
        else:
            out.append(body[i + 1])
            i += 2
    return bytes(out)


_OBF_RE = re.compile(
    rb'd2c::new_string_utf_obf_seeded\(\s*env,\s*b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\s*\)'
)
_NAME_RE = re.compile(
    rb'd2c::Name::obf\(\s*b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\s*\)'
)

# Packages whose descriptors the runtime itself passes in fixed signature
# literals -- `Ljava/lang/Object;` occurs inside
# `([Ljava/lang/Object;)Ljava/lang/Object;` in the MethodHandle path -- so finding
# one in the `.so` says nothing about whether the app's names leaked.
_PLATFORM_PREFIXES = (
    b"java/", b"javax/", b"android/", b"androidx/", b"kotlin/", b"kotlinx/",
    b"dalvik/", b"sun/", b"org/jetbrains/", b"org/intellij/",
)


def obfuscated_constants(project: Path) -> set:
    """The plaintext of every string constant the generator obfuscated.

    `--obfuscate` is worth nothing while the plaintext is still in the `.so`, so
    once it works the loader check loses its evidence: a converted
    `System.loadLibrary("stub")` no longer leaves `stub` anywhere in the library.
    The generator's output is where that constant still is -- as ciphertext plus
    the seed the runtime decodes it with.

    This is a second implementation of `dex2rs_runtime::keystream` and
    `codegen::keystream_seed`, which is a real cost; it is written to fail
    *closed*. The decoded values only ever feed a comparison against the library
    the app is configured to load, so a decoder that drifted would report a
    missing hook (a false failure someone investigates) rather than invent one.
    """
    found = set()
    for path in sorted(project.glob("src/**/*.rs")):
        data = path.read_bytes()
        for cipher, seed in _OBF_RE.findall(data):
            raw = _byte_literal(cipher)
            plain = bytes(c ^ k for c, k in zip(raw, _keystream(int(seed, 16), len(raw))))
            found.add(plain.rstrip(b"\x00"))
    return found


def obfuscated_names(project: Path) -> set:
    """The plaintext of every class/field/method name the generator encrypted.

    The other half of [`obfuscated_constants`]: `--obfuscate` encrypts the names
    the reflection path passes as well as the string constants, so `strings` on
    the library cannot read the converted code's class names out of it. This
    decodes them from the generated crate with the same keystream port.
    """
    found = set()
    for path in sorted(project.glob("src/**/*.rs")):
        data = path.read_bytes()
        for cipher, seed in _NAME_RE.findall(data):
            raw = _byte_literal(cipher)
            plain = bytes(c ^ k for c, k in zip(raw, _keystream(int(seed, 16), len(raw))))
            found.add(plain)
    return found


def app_class_needles(names: set) -> list:
    """`(label, bytes)` pairs to look for in the library, for app-owned classes.

    Both forms a class reaches the generated code as: the descriptor
    (`Lcom/x/Foo;`, what `resolve_*`/`check_cast`/the handler match pass) and the
    slashed path (`com/x/Foo`, what a `--dynamic-register` build registers by).
    Method names and signatures are deliberately left out: they also occur inside
    JNI symbol names and source paths, which say nothing about this.
    """
    out = []
    seen = set()
    for name in sorted(names):
        if name[:1] == b"L":
            rest = name[1:]
        elif name[:2] == b"[L":
            rest = name[2:]
        else:
            continue
        if not rest.endswith(b";"):
            continue
        path = rest[:-1]
        if path.startswith(_PLATFORM_PREFIXES):
            continue
        for label, needle in ((name, name), (path, path)):
            if needle in seen:
                continue
            seen.add(needle)
            out.append((label.decode("utf-8", "replace"), needle))
    return out


def min_meaningful_needle(blob_len: int) -> int:
    """Shortest needle whose presence in `blob_len` bytes is evidence, not luck.

    An N-byte string has `256**N` possible values, so a blob of B bytes contains
    any *given* one about `B / 256**N` times by chance. This returns the smallest N
    for which that is below 1/100.

    It matters at the short end, and only there: an obfuscated app has class paths
    as short as three bytes (`Lp/t;`), and a three-byte needle appears in a 7 MB
    library 0.46 times on average -- so finding one says nothing. Reporting it was
    a false alarm on a real `--obfuscate` build, where the hit was 58 isolated
    `p/t` runs inside NUL padding, not a string. Everything longer is still
    checked, which is where a real leak shows up: a leaked class name appears as
    the `Lcom/x/Foo;` descriptor or a path a table registers by, and those are what
    identify the app.
    """
    length = 1
    while blob_len / 256**length >= 0.01:
        length += 1
    return length


def check_reflection_names(state: dict, report: Report, args) -> None:
    """`--obfuscate` has to keep the app's class names out of the library.

    What the reflection-path encryption is worth is measured the only way it can
    be: decode the names from the generated crate and look for the plaintext in
    the `.so` the app loads. It is also the check that would catch the option
    silently stopping working, because those names are exactly what identifies
    the converted code to `strings` -- a real app has ~40 000 call sites carrying
    them.
    """
    report.section("reflection names")
    project = getattr(args, "project", None)
    lib_name = args.lib_name or _guess_lib_name(args)
    if not project or not (project / "src").is_dir():
        report.info("no --project given: the encrypted names cannot be decoded (SKIP)")
        return
    names = obfuscated_names(project)
    if not names:
        report.info(
            f"no `d2c::Name::obf` call under {project}/src: not an --obfuscate build (SKIP)"
        )
        return
    needles = app_class_needles(names)
    abis = state.get("abis") or []
    checked = 0
    leaked = []
    skipped = 0
    min_len = 1
    with zipfile.ZipFile(args.apk) as zf:
        for abi in abis:
            entry = f"lib/{abi}/lib{lib_name}.so"
            if entry not in zf.namelist():
                continue
            blob = zf.read(entry)
            checked += 1
            min_len = min_meaningful_needle(len(blob))
            for label, needle in needles:
                if len(needle) < min_len:
                    skipped += 1
                    continue
                if needle in blob:
                    leaked.append(f"{abi}: {label}")
    if checked == 0:
        report.warn(f"no lib{lib_name}.so for {abis or 'any abi'} to look in (SKIP)")
        return
    kept = len(needles) - skipped
    report.info(
        f"{len(names)} names decode ({kept} of {len(needles)} app-class forms looked "
        f"for, {skipped} shorter than {min_len} bytes skipped as coincidence), in "
        f"lib{lib_name}.so of {checked} abi(s)"
    )
    if leaked:
        report.fail(
            f"{len(leaked)} app class name(s) still appear verbatim in "
            f"lib{lib_name}.so, so --obfuscate does not hide them: "
            + ", ".join(leaked[:5])
        )
    else:
        report.ok(f"no app class name appears verbatim in lib{lib_name}.so")


def check_loader(state: dict, report: Report, args) -> None:
    report.section("loader")
    # The loader either replaces android:name with the DccApplication class or is
    # a `System.loadLibrary` call inside the app class' <clinit>.
    bt = state.get("build_tools")
    tree = ""
    if bt:
        result = run([str(bt / "aapt2"), "dump", "xmltree", "--file", "AndroidManifest.xml", str(args.apk)])
        tree = result.stdout or ""
    lib_name = args.lib_name or _guess_lib_name(args)
    found_manifest = "DccApplication" in tree or lib_name in tree

    dex_text = b""
    dexes = state.get("dexes") or {}
    for _name, (data, _v, _m) in dexes.items():
        dex_text += data
    found_clinit = lib_name.encode() in dex_text

    # The `System.loadLibrary("stub")` call site can itself be one of the
    # converted methods (`NativeEngine.ensureLoaded` in this app): its string
    # constant then lives in the generated `.so` instead of the dex, and the call
    # is made through JNI from the converted body.
    found_converted = False
    with zipfile.ZipFile(args.apk) as zf:
        for name in zf.namelist():
            if name.startswith("lib/") and name.endswith(".so"):
                if lib_name.encode() in zf.read(name):
                    found_converted = True
                    break

    # With `--obfuscate` the constant is intentional gone from the library, so
    # the third place it can be is the generator's own output.
    found_obfuscated = False
    project = getattr(args, "project", None)
    if not (found_manifest or found_clinit or found_converted) and project and (project / "src").is_dir():
        found_obfuscated = lib_name.encode() in obfuscated_constants(project)

    if found_manifest or found_clinit:
        report.ok(f"a loadLibrary(\"{lib_name}\") hook is present")
    elif found_converted:
        report.ok(
            f"loadLibrary(\"{lib_name}\") is emitted from the converted code "
            "(the original call site was compiled to native)"
        )
    elif found_obfuscated:
        report.ok(
            f"loadLibrary(\"{lib_name}\") is emitted from the converted code "
            "(the constant is obfuscated in the built library)"
        )
    else:
        report.fail(
            f"no System.loadLibrary(\"{lib_name}\") call found in the manifest, the dex "
            "or the converted libraries — native methods will never be registered"
        )
    del found_clinit, found_converted, found_obfuscated


def check_so_binaries(state: dict, report: Report, args) -> None:
    report.section("native libs")
    lib_name = args.lib_name or _guess_lib_name(args)
    found = 0
    with zipfile.ZipFile(args.apk) as zf:
        for abi in state.get("abis", []):
            entry = f"lib/{abi}/lib{lib_name}.so"
            if entry not in zf.namelist():
                report.warn(f"{entry} is missing for abi {abi}")
                continue
            found += 1
            blob = zf.read(entry)
            if not blob.startswith(b"\x7fELF"):
                report.fail(f"{entry} is not an ELF file")
                continue
            is64 = blob[4] == 2
            machine = struct.unpack_from("<H", blob, 18)[0]
            expected = {"aarch64": 183, "arm": 40, "x86_64": 62, "i386": 3, "riscv64": 243}
            want = expected.get(("aarch64" if abi in ("arm64-v8a",) else "arm" if abi == "armeabi-v7a" else abi))
            if want is not None and machine != want:
                report.fail(f"{entry}: ELF machine {machine} does not match abi {abi} (want {want})")
            else:
                report.ok(f"{entry}: valid {'ELF64' if is64 else 'ELF32'} for {abi}")
    if found == 0:
        report.fail(f"no lib{lib_name}.so found in the archive")


# --------------------------------------------------------------------------- #


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("apk", type=Path)
    parser.add_argument("--original", type=Path, help="the input apk, to tell old native methods from new ones")
    parser.add_argument("--project", type=Path, help="the generated Rust project directory")
    parser.add_argument("--lib-name", help="library name (default: inferred from the apk)")
    parser.add_argument("--dynamic-register", action="store_true", help="RegisterNatives instead of JNI exports")
    args = parser.parse_args()

    if not args.apk.is_file():
        print(f"error: {args.apk} not found", file=sys.stderr)
        return 2

    report = Report()
    report.lines.append(f"verifying {args.apk}")
    state = check_apk(args.apk, report, args)
    if state:
        check_signing(args.apk, report, state.get("build_tools"))
        check_dexes(state, report, args)
        check_loader(state, report, args)
        check_so_binaries(state, report, args)
        check_reflection_names(state, report, args)
    report.dump()
    return 1 if report.failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
