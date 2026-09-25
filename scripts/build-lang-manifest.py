#!/usr/bin/env python3
"""Regenerate the committed tessdata manifest.

The manifest is what `lq lang add` verifies every download against, so it has
to be reproducible by anyone who distrusts the copy in the repo: run this
script against the same pinned tag and the file should come out byte-identical
apart from the `generated` date.

Each pack is streamed through SHA-256 and discarded — nothing is written to
disk but the manifest itself, so regenerating costs bandwidth and no storage.

    python scripts/build-lang-manifest.py --tag 4.1.0 -o assets/tessdata-fast-4.1.0.toml

Downloads roughly 340 MB. Takes a few minutes.
"""

import argparse
import concurrent.futures
import datetime
import hashlib
import json
import sys
import urllib.request

REPO = "tesseract-ocr/tessdata_fast"
TREE_API = "https://api.github.com/repos/{repo}/git/trees/{tag}"
RAW_URL = "https://github.com/{repo}/raw/{tag}/{name}"

# Tesseract ships two files that are not languages. `osd` is the hard case: it
# is a legitimate download (orientation and script detection) that must never
# appear in a `lang=` string, and telling those apart is the whole reason this
# column exists.
DETECTORS = {
    "osd": "Orientation and script detection",
    "equ": "Math / equation detection",
}

NAMES = {
    "afr": "Afrikaans", "amh": "Amharic", "ara": "Arabic", "asm": "Assamese",
    "aze": "Azerbaijani", "aze_cyrl": "Azerbaijani (Cyrillic)",
    "bel": "Belarusian", "ben": "Bengali", "bod": "Tibetan", "bos": "Bosnian",
    "bre": "Breton", "bul": "Bulgarian", "cat": "Catalan", "ceb": "Cebuano",
    "ces": "Czech", "chi_sim": "Chinese (Simplified)",
    "chi_sim_vert": "Chinese (Simplified, vertical)",
    "chi_tra": "Chinese (Traditional)",
    "chi_tra_vert": "Chinese (Traditional, vertical)",
    "chr": "Cherokee", "cos": "Corsican", "cym": "Welsh", "dan": "Danish",
    "deu": "German", "div": "Dhivehi", "dzo": "Dzongkha", "ell": "Greek",
    "eng": "English", "enm": "English (Middle, 1100-1500)", "epo": "Esperanto",
    "est": "Estonian", "eus": "Basque", "fao": "Faroese", "fas": "Persian",
    "fil": "Filipino", "fin": "Finnish", "fra": "French",
    "frk": "German (Fraktur)", "frm": "French (Middle, ca. 1400-1600)",
    "fry": "Frisian (Western)", "gla": "Gaelic (Scottish)", "gle": "Irish",
    "glg": "Galician", "grc": "Greek (Ancient, to 1453)", "guj": "Gujarati",
    "hat": "Haitian Creole", "heb": "Hebrew", "hin": "Hindi",
    "hrv": "Croatian", "hun": "Hungarian", "hye": "Armenian",
    "iku": "Inuktitut", "ind": "Indonesian", "isl": "Icelandic",
    "ita": "Italian", "ita_old": "Italian (Old)", "jav": "Javanese",
    "jpn": "Japanese", "jpn_vert": "Japanese (vertical)", "kan": "Kannada",
    "kat": "Georgian", "kat_old": "Georgian (Old)", "kaz": "Kazakh",
    "khm": "Khmer", "kir": "Kyrgyz", "kmr": "Kurdish (Northern)",
    "kor": "Korean", "kor_vert": "Korean (vertical)", "lao": "Lao",
    "lat": "Latin", "lav": "Latvian", "lit": "Lithuanian",
    "ltz": "Luxembourgish", "mal": "Malayalam", "mar": "Marathi",
    "mkd": "Macedonian", "mlt": "Maltese", "mon": "Mongolian",
    "mri": "Maori", "msa": "Malay", "mya": "Burmese", "nep": "Nepali",
    "nld": "Dutch", "nor": "Norwegian", "oci": "Occitan", "ori": "Oriya",
    "pan": "Punjabi", "pol": "Polish", "por": "Portuguese", "pus": "Pashto",
    "que": "Quechua", "ron": "Romanian", "rus": "Russian", "san": "Sanskrit",
    "sin": "Sinhala", "slk": "Slovak", "slv": "Slovenian", "snd": "Sindhi",
    "spa": "Spanish", "spa_old": "Spanish (Old)", "sqi": "Albanian",
    "srp": "Serbian", "srp_latn": "Serbian (Latin)", "sun": "Sundanese",
    "swa": "Swahili", "swe": "Swedish", "syr": "Syriac", "tam": "Tamil",
    "tat": "Tatar", "tel": "Telugu", "tgk": "Tajik", "tha": "Thai",
    "tir": "Tigrinya", "ton": "Tongan", "tur": "Turkish", "uig": "Uyghur",
    "ukr": "Ukrainian", "urd": "Urdu", "uzb": "Uzbek",
    "uzb_cyrl": "Uzbek (Cyrillic)", "vie": "Vietnamese", "yid": "Yiddish",
    "yor": "Yoruba",
}


def fetch(url: str) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": "lensquery-manifest"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        return resp.read()


def list_packs(tag: str) -> list[str]:
    tree = json.loads(fetch(TREE_API.format(repo=REPO, tag=tag) + "?recursive=1"))
    if tree.get("truncated"):
        sys.exit("tree listing was truncated; cannot build a complete manifest")
    return sorted(
        e["path"][: -len(".traineddata")]
        for e in tree["tree"]
        if e["path"].endswith(".traineddata") and "/" not in e["path"]
    )


def digest(tag: str, code: str) -> tuple[str, str, int]:
    url = RAW_URL.format(repo=REPO, tag=tag, name=code + ".traineddata")
    req = urllib.request.Request(url, headers={"User-Agent": "lensquery-manifest"})
    h = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(req, timeout=300) as resp:
        while chunk := resp.read(1 << 20):
            h.update(chunk)
            size += len(chunk)
    return code, h.hexdigest(), size


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", default="4.1.0")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("-o", "--output", required=True)
    args = ap.parse_args()

    codes = list_packs(args.tag)
    print(f"{len(codes)} packs at {REPO}@{args.tag}", file=sys.stderr)

    rows = {}
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        futures = [pool.submit(digest, args.tag, c) for c in codes]
        for i, fut in enumerate(concurrent.futures.as_completed(futures), 1):
            code, sha, size = fut.result()
            rows[code] = (sha, size)
            print(f"  [{i}/{len(codes)}] {code} {size}", file=sys.stderr)

    today = datetime.date.today().isoformat()
    with open(args.output, "w", encoding="utf-8", newline="\n") as f:
        f.write(
            "# Generated by scripts/build-lang-manifest.py. Do not edit by hand.\n"
            "#\n"
            "# Every entry is verified by `lq lang add` after download; a mismatch\n"
            "# refuses the install. Regenerate with:\n"
            f"#     python scripts/build-lang-manifest.py --tag {args.tag} -o {args.output}\n"
            "\n"
            f'source = "https://github.com/{REPO}"\n'
            f'tag = "{args.tag}"\n'
            f'generated = "{today}"\n'
        )
        for code in codes:
            sha, size = rows[code]
            name = DETECTORS.get(code) or NAMES.get(code, code)
            kind = "detector" if code in DETECTORS else "language"
            f.write(
                "\n[[pack]]\n"
                f'code = "{code}"\n'
                f'name = "{name}"\n'
                f'kind = "{kind}"\n'
                f"size = {size}\n"
                f'sha256 = "{sha}"\n'
            )
    missing = [c for c in codes if c not in NAMES and c not in DETECTORS]
    if missing:
        print(f"no display name for: {' '.join(missing)}", file=sys.stderr)
    print(f"wrote {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
