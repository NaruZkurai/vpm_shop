#!/usr/bin/env python3
"""
gen_category_repos.py
─────────────────────
Splits the master vpm-repo.json into per-category repos:
  VPM/<Category>/vpm-repo.json

Behavior is driven by a gitignored config file (repos.conf), so personal
domains and editable categories never live in this source file. See
repos.conf.example for the format.

Run this whenever packages are added or moved.
"""

import json, os
from pathlib import Path

SHOP      = Path(os.environ.get("VPM_SHOP", "."))      # where the shop lives
REPO_JSON = SHOP / "vpm-repo.json"
VPM_DIR   = SHOP / "VPM"
CONF_PATH = Path(os.environ.get("REPOS_CONF", SHOP / "repos.conf"))

# ── Config (no defaults here — everything comes from repos.conf) ──────────
def load_conf() -> dict:
    """Load repos.conf (gitignored). Copy repos.conf.example to repos.conf and
    fill in your domain / categories. Fails hard if missing — this repo is the
    source of truth for config, so there are no silent fallback values."""
    if not CONF_PATH.is_file():
        raise SystemExit(
            f"ERROR: config not found at {CONF_PATH}\n"
            f"  Copy repos.conf.example to repos.conf and set your domain + categories,\n"
            f"  then rerun.\n"
        )
    with open(CONF_PATH) as f:
        conf = json.load(f)
    for key in ("master_host", "subdomain_host", "author", "master_name", "master_id", "categories"):
        if key not in conf:
            raise SystemExit(f"ERROR: repos.conf is missing required key '{key}'")
    return conf


def conf_url(pattern: str, sub: str | None = None) -> str:
    """Expand a URL template; '{sub}' becomes the category subdomain."""
    if "{sub}" in pattern:
        return pattern.replace("{sub}", sub or "misc")
    return pattern


def main():
    conf = load_conf()
    MASTER_HOST = conf["master_host"]
    SUBDOMAIN   = {c["name"]: c.get("sub", "misc") for c in conf["categories"]}
    CATEGORY_META = {
        c["name"]: {"name": c.get("repo_name"),
                    "id":   c.get("repo_id")}
        for c in conf["categories"]
    }

    with open(REPO_JSON) as f:
        master = json.load(f)

    def category_url(cat: str, master_url: str) -> str:
        """Rewrite a master URL to its category subdomain when it lives in that folder."""
        sub = SUBDOMAIN.get(cat, "misc")
        prefix = f"{MASTER_HOST}/{cat}/"
        if master_url.startswith(prefix):
            return conf_url(conf["subdomain_host"], sub) + "/" + master_url[len(prefix):]
        return master_url

    # ── Save updated master repo ─────────────────────────────────────────────
    with open(REPO_JSON, "w") as f:
        json.dump(master, f, indent=2, ensure_ascii=False)

    # ── Publish master repo where vpm-shop serves it ─────────────────────────
    master_published = {
        "name":     master.get("name", conf["master_name"]),
        "id":       master.get("id", conf["master_id"]),
        "url":      conf_url(conf["subdomain_host"], "master") + "/index.json",
        "author":   master.get("author", conf["author"]),
        "packages": master["packages"],
    }
    master_path = VPM_DIR / "vpm-repo.json"
    with open(master_path, "w") as f:
        json.dump(master_published, f, indent=2, ensure_ascii=False)
    print(f"  wrote {master_path.relative_to(SHOP)}  "
          f"({len(master['packages'])} pkg, master)")

    # ── Group packages by category ───────────────────────────────────────────
    categories: dict[str, dict] = {}

    for pkg_id, pkg in master["packages"].items():
        cat = next((e.get("category", "Misc") for e in pkg["versions"].values()), "Misc")
        categories.setdefault(cat, {})[pkg_id] = pkg

    # ── Write per-category repo JSON ─────────────────────────────────────────
    repo_urls: list[str] = []

    for cat, packages in sorted(categories.items()):
        meta = CATEGORY_META.get(cat)
        if meta is None or meta["id"] is None:
            # Auto-register a brand-new category in repos.conf so the user can
            # see and rename it later instead of silently guessing.
            sub = "".join(ch for ch in cat.lower() if ch.isalnum()) or "misc"
            entry = {
                "name": cat,
                "sub": sub,
                "repo_name": f"VPM – {cat}",
                "repo_id": f"com.vpm.{sub}",
            }
            conf["categories"].append(entry)
            SUBDOMAIN[cat] = sub
            CATEGORY_META[cat] = {"name": entry["repo_name"], "id": entry["repo_id"]}
            with open(CONF_PATH, "w") as f:
                json.dump(conf, f, indent=2, ensure_ascii=False)
            print(f"  [config] added new category '{cat}' to {CONF_PATH} (edit it anytime)")
            meta = CATEGORY_META[cat]
        cat_dir = VPM_DIR / cat
        cat_dir.mkdir(parents=True, exist_ok=True)
        repo_url = conf_url(conf["subdomain_host"], SUBDOMAIN.get(cat, "misc")) + "/index.json"

        # Rewrite zip URLs to the category subdomain where the zip lives in this folder
        for pkg in packages.values():
            for ver_entry in pkg["versions"].values():
                ver_entry["url"] = category_url(cat, ver_entry["url"])

        cat_repo = {
            "name":     meta["name"],
            "id":       meta["id"],
            "url":      repo_url,
            "author":   conf["author"],
            "packages": packages,
        }
        repo_path = cat_dir / "vpm-repo.json"
        with open(repo_path, "w") as f:
            json.dump(cat_repo, f, indent=2, ensure_ascii=False)
        pkg_count = len(packages)
        ver_count = sum(len(p["versions"]) for p in packages.values())
        print(f"  wrote {repo_path.relative_to(SHOP)}  ({pkg_count} pkg, {ver_count} ver)")
        repo_urls.append(repo_url)

    # ── Write repos-list.txt ─────────────────────────────────────────────────
    repos_list = SHOP / "repos-list.txt"
    with open(repos_list, "w") as f:
        f.write("# VPM Repos – add each URL to ALCOM\n")
        f.write("# ALCOM → Settings → Packages → User Repositories → Add\n\n")
        for url in sorted(repo_urls):
            f.write(url + "\n")

    print(f"\n  repos-list.txt written with {len(repo_urls)} repo URLs.")
    print(f"\n  ─ Paste into ALCOM ─")
    for url in sorted(repo_urls):
        print(f"    {url}")


if __name__ == "__main__":
    main()
