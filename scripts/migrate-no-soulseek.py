#!/usr/bin/env python3
"""Moves an Octo install's state and settings off Soulseek, once, before the first start of an
Octo without it.

What changes, and nothing else:
- The provider name in song keys and ids: `soulseek:<id>` → `outside:<id>`,
  `ext-soulseek-<kind>-<id>` → `ext-outside-<kind>-<id>`, and the `ExternalProvider` /
  `Provider` field "soulseek" → "outside". These are read from `downloads-history.json`,
  `imports-trickle.json`, `upgrades.json`, `lastfm-radio-state.json`, `family-playlists.json`,
  `imports.json` and the music folder's `.mappings.json`.
- Downloads that came from Soulseek leave the history; Soulseek copies leave download logs and
  upgrade picks; trickle notes that named Soulseek are reworded.
- `settings.json`: Soulseek leaves the heart download sources (YouTube is the default when none
  is left), `DownloadSource` becomes YouTube, `WaitForLosslessOnPlay`, `LosslessWaitTimeoutSeconds`,
  `NotifyLosslessFallback` and a Soulseek `UpgradeSource` go, and the `Soulseek` section's
  download-wide keys (verification, fingerprints, AcoustID, parallel downloads, transcode
  detection, naming) move to the new `Downloads` section; the rest of the section goes.
- `soulseek-holds.json` and `rejected-peers.json` are deleted.

Every file it changes is copied to `<name>.before-no-soulseek` first (contents only: a share
that refuses chmod must not stop the copy). Run it again and it finds
nothing to do.

    python3 migrate-no-soulseek.py --config /app/config --music /music [--dry-run]
"""
import argparse
import json
import os
import re
import shutil
import sys

OLD = "soulseek"
NEW = "outside"
KEY_PREFIX = re.compile(r"^soulseek:", re.IGNORECASE)
EXT_ID = re.compile(r"ext-soulseek-", re.IGNORECASE)
PROVIDER_FIELDS = {"ExternalProvider", "Provider", "provider", "external_provider"}
DOWNLOADS_KEYS = [
    "VerifyDownloads",
    "AcoustIdApiKey",
    "AcoustIdUserApiKey",
    "SubmitConfirmedFingerprints",
    "MinMatchScore",
    "TagFromMusicBrainz",
    "FingerprintSeconds",
    "FingerprintTimeoutSeconds",
    "AcoustIdTimeoutSeconds",
    "NameFromMatch",
    "ParallelDownloads",
    "DetectTranscodes",
]
STATE_FILES = [
    "downloads-history.json",
    "imports-trickle.json",
    "upgrades.json",
    "lastfm-radio-state.json",
    "family-playlists.json",
    "imports.json",
]
GONE_FILES = ["soulseek-holds.json", "rejected-peers.json"]


class Count:
    def __init__(self):
        self.n = 0

    def hit(self, by=1):
        self.n += by


def rename_ids(node, count, parent_key=None):
    """Walks a JSON value, renaming the provider in keys, ids and provider fields."""
    if isinstance(node, dict):
        out = {}
        for key, value in node.items():
            new_key = key
            if isinstance(key, str) and KEY_PREFIX.match(key):
                new_key = KEY_PREFIX.sub(NEW + ":", key)
                count.hit()
            out[new_key] = rename_ids(value, count, new_key)
        return out
    if isinstance(node, list):
        return [rename_ids(item, count, parent_key) for item in node]
    if isinstance(node, str):
        text = node
        if parent_key in PROVIDER_FIELDS and text.lower() == OLD:
            count.hit()
            return NEW
        if KEY_PREFIX.match(text):
            count.hit()
            text = KEY_PREFIX.sub(NEW + ":", text)
        if EXT_ID.search(text):
            count.hit()
            text = EXT_ID.sub("ext-" + NEW + "-", text)
        return text
    return node


def is_soulseek_source(entry):
    return isinstance(entry, dict) and str(entry.get("Source") or entry.get("source") or "").lower() == OLD


def strip_soulseek_copies(node, count):
    """Takes Soulseek copies out of candidate lists and picks, wherever they sit."""
    if isinstance(node, dict):
        out = {}
        for key, value in node.items():
            if key in ("Candidates", "candidates") and isinstance(value, list):
                kept = [c for c in value if not is_soulseek_source(c)]
                count.hit(len(value) - len(kept))
                out[key] = [strip_soulseek_copies(c, count) for c in kept]
            elif key in ("Pick", "pick") and is_soulseek_source(value):
                count.hit()
                out[key] = None
            else:
                out[key] = strip_soulseek_copies(value, count)
        return out
    if isinstance(node, list):
        return [strip_soulseek_copies(item, count) for item in node]
    if isinstance(node, str) and "soulseek" in node.lower():
        count.hit()
        return re.sub(r"\bon Soulseek\b", "from the download source", node, flags=re.IGNORECASE).replace("Soulseek", "the download source").replace("soulseek", "the download source")
    return node


def migrate_history(data, report):
    if not isinstance(data, list):
        return data
    kept = [e for e in data if not is_soulseek_source(e)]
    if len(kept) != len(data):
        report["downloads from Soulseek dropped"] = len(data) - len(kept)
    return kept


def migrate_settings(data, report):
    if not isinstance(data, dict):
        return data
    subsonic = data.get("Subsonic")
    if isinstance(subsonic, dict):
        sources = subsonic.get("HeartDownloadSources")
        if isinstance(sources, list):
            kept = [s for s in sources if not is_soulseek_source(s)]
            if len(kept) != len(sources):
                report["heart download sources: Soulseek dropped"] = len(sources) - len(kept)
                subsonic["HeartDownloadSources"] = kept
        if str(subsonic.get("DownloadSource", "")).lower().startswith("soulseek"):
            subsonic["DownloadSource"] = "YouTube"
            report["DownloadSource → YouTube"] = 1
        for key in ("WaitForLosslessOnPlay", "LosslessWaitTimeoutSeconds"):
            if key in subsonic:
                del subsonic[key]
                report[f"Subsonic.{key} removed"] = 1
    notifications = data.get("Notifications")
    if isinstance(notifications, dict) and "NotifyLosslessFallback" in notifications:
        del notifications["NotifyLosslessFallback"]
        report["Notifications.NotifyLosslessFallback removed"] = 1
    actions = data.get("LibraryActions")
    if isinstance(actions, dict) and str(actions.get("UpgradeSource", "")).lower() == OLD:
        actions["UpgradeSource"] = "Auto"
        report["LibraryActions.UpgradeSource → Auto"] = 1
    soulseek = data.pop("Soulseek", None)
    if isinstance(soulseek, dict):
        moved = {k: soulseek[k] for k in DOWNLOADS_KEYS if k in soulseek}
        if moved:
            downloads = data.setdefault("Downloads", {})
            for key, value in moved.items():
                downloads.setdefault(key, value)
        report["Soulseek section: keys moved to Downloads"] = len(moved)
        report["Soulseek section: keys dropped"] = len(soulseek) - len(moved)
    return data


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def save(path, data, dry_run):
    if dry_run:
        return
    backup = path + ".before-no-soulseek"
    if not os.path.exists(backup):
        shutil.copyfile(path, backup)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
    os.replace(tmp, path)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", required=True, help="Octo's config folder (settings.json and the state files)")
    parser.add_argument("--music", help="the music folder (for .mappings.json)")
    parser.add_argument("--dry-run", action="store_true", help="say what would change, change nothing")
    args = parser.parse_args()

    files = [os.path.join(args.config, name) for name in STATE_FILES + ["settings.json"]]
    if args.music:
        files.append(os.path.join(args.music, ".mappings.json"))
    changed_any = False
    for path in files:
        if not os.path.isfile(path):
            continue
        try:
            data = load(path)
        except (OSError, ValueError) as e:
            print(f"{path}: could not read ({e}); left alone", file=sys.stderr)
            continue
        report = {}
        ids, copies = Count(), Count()
        name = os.path.basename(path)
        if name == "settings.json":
            data = migrate_settings(data, report)
        else:
            if name == "downloads-history.json":
                data = migrate_history(data, report)
            data = rename_ids(data, ids)
            data = strip_soulseek_copies(data, copies)
            if ids.n:
                report["ids renamed"] = ids.n
            if copies.n:
                report["Soulseek copies and notes removed"] = copies.n
        if report:
            changed_any = True
            print(f"{path}: " + ", ".join(f"{k}: {v}" for k, v in report.items()))
            save(path, data, args.dry_run)
    for name in GONE_FILES:
        path = os.path.join(args.config, name)
        if os.path.isfile(path):
            changed_any = True
            print(f"{path}: deleted")
            if not args.dry_run:
                backup = path + ".before-no-soulseek"
                if not os.path.exists(backup):
                    shutil.copyfile(path, backup)
                os.remove(path)
    if not changed_any:
        print("Nothing to do.")
    elif args.dry_run:
        print("(dry run: nothing written)")


if __name__ == "__main__":
    main()
