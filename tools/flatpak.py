#!/usr/bin/env python3
"""Signet's Flatpak: build it from the commit tagged v<version>, check it, and make the signed
Flatpak repository it's installed from. Same layout as Sealwax's (PixDevsApps/sealwax).

  python3 tools/flatpak.py keygen   the repository's signing key, once, in .local/flatpak/gpg or
                                    SIGNET_FLATPAK_GPG_HOME (never committed; back it up: a lost
                                    key means everyone has to add the repository again)
  python3 tools/flatpak.py build    flatpak-builder on `git archive HEAD` with
                                    packaging/flatpak/io.brem.Signet.yml, offline, into an
                                    unsigned repository .local/flatpak/release/<version>/repo,
                                    then checked (--untagged: build HEAD anyway, for a test)
  python3 tools/flatpak.py site     that build signed into .local/flatpak/site/repo with static
                                    deltas, plus signet.flatpakrepo, io.brem.Signet.flatpakref
                                    and index.html; needs SIGNET_FLATPAK_URL (https://..., ending
                                    in /), where GitHub Pages serves the site
  python3 tools/flatpak.py publish  the site force-pushed, as one commit, to the Pages repository
                                    (SIGNET_FLATPAK_PAGES_REPO); attach the .flatpakref to the
                                    GitHub release too. Publishes: run it only on the owner's word.

Tools missing on the host (flatpak-builder, ostree, appstreamcli) run from the org.flatpak.Builder
flatpak: flatpak install --user flathub org.flatpak.Builder
"""
import base64
import os
import pathlib
import shutil
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
APP_ID = "io.brem.Signet"
BRANCH = "stable"
ARCH = "x86_64"
MANIFEST = pathlib.Path("packaging/flatpak") / f"{APP_ID}.yml"
WORK = ROOT / ".local" / "flatpak"
GPG_HOME = pathlib.Path(os.environ.get("SIGNET_FLATPAK_GPG_HOME") or WORK / "gpg")
SITE = WORK / "site"
KEY_NAME = "Signet Flatpak repository"
HOMEPAGE = "https://github.com/etmartinkazoo/signet"
RUNTIME_REPO = "https://dl.flathub.org/repo/flathub.flatpakrepo"
PAGES_REPO = os.environ.get("SIGNET_FLATPAK_PAGES_REPO", "https://github.com/etmartinkazoo/signet-flatpak.git")
RELEASE_REF = f"{HOMEPAGE}/releases/latest/download/{APP_ID}.flatpakref"


def tool(name: str) -> list[str]:
    """The command for `name`: the host's, else the org.flatpak.Builder flatpak's."""
    if shutil.which(name):
        return [name]
    # The wrapper points flatpak-builder at the user's Flatpak installation (SDK, extensions).
    command = "flatpak-builder-wrapper" if name == "flatpak-builder" else name
    return ["flatpak", "run", f"--command={command}", "org.flatpak.Builder"]


def run(*args: str, cwd: pathlib.Path = ROOT) -> None:
    print("+", " ".join(args), flush=True)
    subprocess.run(args, cwd=cwd, check=True)


def output(*args: str) -> str:
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def version() -> str:
    return tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def release_dir(ver: str) -> pathlib.Path:
    return WORK / "release" / ver


def check_head(ver: str) -> None:
    if "--untagged" in sys.argv[2:]:
        return
    if output("git", "status", "--porcelain", "--untracked-files=no"):
        raise SystemExit("the working tree has uncommitted changes")
    try:
        tagged = output("git", "rev-parse", f"v{ver}^{{commit}}")
    except subprocess.CalledProcessError:
        raise SystemExit(f"no tag v{ver}: tag the release commit first (or --untagged)")
    if tagged != output("git", "rev-parse", "HEAD"):
        raise SystemExit(f"HEAD isn't v{ver}")


def build(ver: str) -> None:
    check_head(ver)
    out = release_dir(ver)
    if out.exists():
        shutil.rmtree(out)
    src = out / "src"
    src.mkdir(parents=True)
    archive = subprocess.run(["git", "archive", "--format=tar", "HEAD"], cwd=ROOT, check=True,
                             capture_output=True).stdout
    subprocess.run(["tar", "-x", "-C", str(src)], input=archive, check=True)
    run(sys.executable, "-I", "tools/flatpak-cargo-sources.py", "--check", cwd=src)
    # --user: the GNOME SDK and Rust extension come from the user's installation.
    run(*tool("flatpak-builder"), "--user", "--force-clean", "--disable-rofiles-fuse",
        f"--state-dir={WORK / 'state'}", f"--default-branch={BRANCH}",
        f"--repo={out / 'repo'}", str(out / "build"), str(src / MANIFEST))
    check(ver)


def check(ver: str) -> None:
    files = release_dir(ver) / "build" / "files"
    metainfo = files / "share" / "metainfo" / f"{APP_ID}.metainfo.xml"
    desktop = files / "share" / "applications" / f"{APP_ID}.desktop"
    binary = files / "bin" / "signet"
    for path in (metainfo, desktop, binary):
        if not path.exists():
            raise SystemExit(f"the build has no {path.relative_to(files)}")
    run(*tool("appstreamcli"), "validate", "--no-net", "--explain", str(metainfo))
    if f'<release version="{ver}"' not in metainfo.read_text():
        raise SystemExit(f"{metainfo.name} has no <release> for {ver}")
    run(*tool("desktop-file-validate"), str(desktop))
    if str(pathlib.Path.home()).encode() in binary.read_bytes():
        raise SystemExit("/app/bin/signet holds this machine's home path")
    print(f"checked: {metainfo.name}, {desktop.name}, /app/bin/signet")


def gpg(*args: str) -> str:
    return output("gpg", "--homedir", str(GPG_HOME), "--batch", *args)


def key_id() -> str:
    if not GPG_HOME.exists():
        raise SystemExit("no signing key: python3 tools/flatpak.py keygen")
    for line in gpg("--with-colons", "--list-secret-keys").splitlines():
        fields = line.split(":")
        if fields[0] == "fpr":
            return fields[9]
    raise SystemExit(f"no secret key in {GPG_HOME}")


def keygen() -> None:
    if GPG_HOME.exists() and any(GPG_HOME.iterdir()):
        raise SystemExit(f"{GPG_HOME} already holds a key; remove it by hand to start over")
    GPG_HOME.mkdir(parents=True, exist_ok=True)
    GPG_HOME.chmod(0o700)
    gpg("--passphrase", "", "--quick-gen-key", KEY_NAME, "rsa4096", "sign", "never")
    print(f"signing key {key_id()} in {GPG_HOME}: back the folder up somewhere safe")


def public_key() -> str:
    exported = subprocess.check_output(["gpg", "--homedir", str(GPG_HOME), "--batch", "--export", key_id()])
    return base64.b64encode(exported).decode()


def site(ver: str) -> None:
    url = os.environ.get("SIGNET_FLATPAK_URL", "")
    if not url.startswith("https://") or not url.endswith("/"):
        raise SystemExit("SIGNET_FLATPAK_URL must be the site's https:// address, ending in /")
    source = release_dir(ver) / "repo"
    if not source.exists():
        raise SystemExit(f"run build first: {source} is missing")
    signing = [f"--gpg-sign={key_id()}", f"--gpg-homedir={GPG_HOME}"]
    repo = SITE / "repo"
    if not repo.exists():
        repo.mkdir(parents=True)
        run(*tool("ostree"), "init", "--mode=archive-z2", f"--repo={repo}")
    run(*tool("flatpak"), "build-commit-from", *signing, f"--src-repo={source}", str(repo),
        f"app/{APP_ID}/{ARCH}/{BRANCH}")
    run(*tool("flatpak"), "build-update-repo", *signing, "--generate-static-deltas", "--prune",
        "--prune-depth=3", "--title=Signet", f"--homepage={HOMEPAGE}", f"--default-branch={BRANCH}",
        str(repo))
    key = public_key()
    common = f"Url={url}repo/\nHomepage={HOMEPAGE}\nGPGKey={key}\n"
    (SITE / "signet.flatpakrepo").write_text(
        f"[Flatpak Repo]\nTitle=Signet\nComment=Signet, the calendar, from its own repository\n"
        f"DefaultBranch={BRANCH}\n{common}"
    )
    (SITE / f"{APP_ID}.flatpakref").write_text(
        f"[Flatpak Ref]\nTitle=Signet\nName={APP_ID}\nBranch={BRANCH}\nSuggestRemoteName=signet\n"
        f"IsRuntime=false\nRuntimeRepo={RUNTIME_REPO}\n{common}"
    )
    (SITE / "index.html").write_text(
        f"<!doctype html>\n<meta charset=utf-8><meta name=viewport content='width=device-width'>\n"
        f"<title>Signet Flatpak repository</title>\n<h1>Signet {ver}</h1>\n"
        f"<p>The Signet calendar as a Flatpak. Install it, with its GNOME runtime from Flathub:</p>\n"
        f"<pre>flatpak install --user {RELEASE_REF}</pre>\n"
        f"<p>The repository: <a href=signet.flatpakrepo>signet.flatpakrepo</a> · "
        f"<a href={APP_ID}.flatpakref>{APP_ID}.flatpakref</a></p>\n"
    )
    (SITE / ".nojekyll").write_text("")
    print(f"site for {ver} in {SITE}; serve it at {url}; attach {SITE / (APP_ID + '.flatpakref')} "
          f"to the v{ver} GitHub release")


def publish(ver: str) -> None:
    index = SITE / "index.html"
    if not index.exists() or f"Signet {ver}<" not in index.read_text():
        raise SystemExit(f"run site first: {SITE} isn't {ver}'s")
    pages = WORK / "pages"
    if pages.exists():
        shutil.rmtree(pages)
    shutil.copytree(SITE, pages)
    run("git", "init", "--quiet", "--initial-branch=main", cwd=pages)
    # The site's commit is authored as the Signet repository's own commits are.
    for key in ("user.name", "user.email"):
        run("git", "config", key, output("git", "config", key), cwd=pages)
    run("git", "add", "--all", cwd=pages)
    run("git", "commit", "--quiet", "--message", f"Signet {ver}", cwd=pages)
    run("git", "push", "--force", PAGES_REPO, "main", cwd=pages)


def main() -> int:
    commands = {"keygen": keygen, "build": build, "check": check, "site": site, "publish": publish}
    if len(sys.argv) < 2 or sys.argv[1] not in commands:
        print(__doc__, file=sys.stderr)
        return 2
    command = commands[sys.argv[1]]
    if command is keygen:
        keygen()
    else:
        command(version())
    return 0


if __name__ == "__main__":
    sys.exit(main())
