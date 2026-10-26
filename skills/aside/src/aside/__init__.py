"""aside: drive the Aside Browser (https://aside.com) from the Prime Agent kernel.

Three ways in, cheapest first:

    tabs  = await aside.tabs()                      # what's open right now
    text  = await aside.read("https://example.com") # deterministic extraction
    png   = await aside.screenshot(path="/tmp/p.png")  # then attach_image to SEE it
    out   = await aside.repl("const p = await attachActiveBrowserTab();"
                             "console.log(await p.title())")      # any JS
    answer = await aside("Log into X and tell me the latest DM")  # LLM agent session

`repl` is deterministic Playwright-style automation with no LLM: fast, cheap,
reproducible. `run`/`agent` spends tokens and browser turns, so reach for it only
when the task genuinely needs judgment.

This wrapper exists because the raw CLI has three sharp edges, all verified:
  1. `aside repl` exits 0 even when the JavaScript throws - the only failure signal
     is an `[error | Nms]` footer in stdout. Here that raises AsideReplError.
  2. Output carries ANSI escapes even under NO_COLOR, plus `openTab` status lines,
     so naive parsing breaks. Everything below is stripped and, optionally, JSON-parsed.
  3. The REPL filesystem is sandboxed: `page.screenshot({path: "/tmp/x.png"})`
     silently redirects into the Aside session dir and your path stays empty.
     `screenshot()` ships bytes back over base64 and writes them itself.
"""

from __future__ import annotations

import asyncio
import base64
import json as _json
import os
import re
import shutil
import subprocess
import time
import urllib.request
from pathlib import Path
from typing import Any

__all__ = [
    "run", "agent", "repl", "read", "screenshot", "tabs", "health", "models",
    "providers", "AsideError", "AsideNotRunning", "AsideReplError", "AsideTimeout",
    "CONFIG_DIR", "config_dir",
]

# Aside's per-account config dir for the default account. `models.json` there
# declares custom providers and is hot-reloaded, so edits apply without
# restarting the app. Other accounts live beside it as `~/.aside/u/<n>`; pass
# `account="u1"` to the config readers below to target one of them.
CONFIG_DIR = Path(os.environ.get("ASIDE_CONFIG_DIR") or (Path.home() / ".aside" / "u" / "0"))


def config_dir(account: str | None = None) -> Path:
    """Config dir for an Aside account: `CONFIG_DIR` for the default, else
    `~/.aside/u/<n>` for `account="u<n>"` (a bare number is accepted too)."""
    if not account:
        return CONFIG_DIR
    slot = account[1:] if account.startswith("u") else account
    if not slot.isdigit():
        raise AsideError(f"invalid Aside account id {account!r}; expected e.g. 'u1'")
    return CONFIG_DIR.parent / slot

# Hosts treated as "a gateway running on this machine" when probing provider health.
_LOOPBACK = ("127.0.0.1", "localhost", "0.0.0.0", "::1")

DEFAULT_REPL_TIMEOUT = 120.0
DEFAULT_AGENT_TIMEOUT = 900.0

_ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
_FOOTER = re.compile(r"^\[(ok|error)\s*\|\s*(\d+)ms\]\s*$")
_NOISE = re.compile(r"^(\u2714\ufe0e|\u2714|\u2713|\[WARN\]|\(node:\d+\))")


class AsideError(RuntimeError):
    """Base class for every failure raised by this skill."""


class AsideNotRunning(AsideError):
    """The Aside CLI or the Aside Browser app is unavailable."""


class AsideReplError(AsideError):
    """The REPL reported `[error | Nms]` (the CLI still exits 0, so we raise)."""


class AsideTimeout(AsideError):
    """A bounded aside invocation exceeded its timeout."""


# --------------------------------------------------------------------------
# process plumbing
# --------------------------------------------------------------------------

def _bin() -> str:
    """Resolve the real Aside binary, never a same-named shim on PATH."""
    for cand in (os.environ.get("ASIDE_BIN"), str(Path.home() / ".local/bin/aside"), shutil.which("aside")):
        if cand and os.path.isfile(cand) and os.access(cand, os.X_OK):
            return cand
    raise AsideNotRunning(
        "aside CLI not found. Expected ~/.local/bin/aside - install it or set ASIDE_BIN."
    )


def _app_running() -> bool:
    try:
        out = subprocess.run(["pgrep", "-f", "Aside.app"], capture_output=True, text=True, timeout=10)
        return bool(out.stdout.strip())
    except Exception:
        return False


def _clean(text: str) -> str:
    return _ANSI.sub("", text or "")


def _invoke(args: list[str], timeout: float, label: str) -> tuple[str, str, int, float]:
    env = dict(os.environ)
    env["FORCE_COLOR"] = "0"   # silences the node NO_COLOR warning on stderr
    env["NO_COLOR"] = "1"
    started = time.monotonic()
    try:
        proc = subprocess.run([_bin(), *args], capture_output=True, text=True, timeout=timeout, env=env)
    except subprocess.TimeoutExpired as exc:
        elapsed = time.monotonic() - started
        partial = _clean(exc.stdout.decode() if isinstance(exc.stdout, bytes) else (exc.stdout or ""))
        raise AsideTimeout(
            f"{label} exceeded {timeout:.0f}s (elapsed {elapsed:.1f}s). "
            f"Raise timeout= or use a repl call instead of an agent session.\n"
            f"--- partial output ---\n{partial[-2000:]}"
        ) from None
    return _clean(proc.stdout), _clean(proc.stderr), proc.returncode, time.monotonic() - started


def _parse_repl(stdout: str) -> tuple[str, str | None, int | None]:
    """Split REPL stdout into (body, status, ms), dropping footer and status noise."""
    body, status, ms = [], None, None
    for line in stdout.splitlines():
        m = _FOOTER.match(line.strip())
        if m:
            status, ms = m.group(1), int(m.group(2))
            continue
        if _NOISE.match(line.strip()):
            continue
        body.append(line)
    return "\n".join(body).strip(), status, ms


# --------------------------------------------------------------------------
# core API
# --------------------------------------------------------------------------

async def repl(
    js: str,
    *,
    timeout: float = DEFAULT_REPL_TIMEOUT,
    parse: bool = False,
    account: str | None = None,
) -> Any:
    """Run deterministic JavaScript in the Aside Browser - no LLM, no tokens.

    The code is passed as a single argv element, so multi-line JS containing any
    mix of quotes and backticks is safe with no escaping. Top-level `await` works.
    Only `console.log(...)` is echoed; bare expressions are not.

    Args:
        js: JavaScript source. Globals include openTab, page, tabs, listBrowserTabs,
            snapshot, cua, fs, gmail, notion, slack, twitter, youtube, googleSheets,
            aside.pdf/.sessions/.settings - see references/repl-api.md.
        timeout: hard bound in seconds; raises AsideTimeout past it.
        parse: if True, find the last JSON object/array printed and return it parsed.
        account: Aside account id, e.g. "u1".

    Raises:
        AsideReplError: the REPL printed `[error | Nms]` (exit code is 0 regardless).
        AsideTimeout: exceeded `timeout`.
    """
    args = ["repl"]
    if account:
        args += ["--account", account]
    args.append(js)
    stdout, stderr, _, _ = await asyncio.to_thread(_invoke, args, timeout, "aside repl")
    body, status, _ms = _parse_repl(stdout)
    if status == "error":
        raise AsideReplError(f"{body or stderr.strip() or 'unknown REPL error'}")
    if not parse:
        return body
    for line in reversed(body.splitlines()):
        s = line.strip()
        if s.startswith(("{", "[")):
            try:
                return _json.loads(s)
            except ValueError:
                continue
    raise AsideReplError(f"parse=True but no JSON line found in output:\n{body[:500]}")


async def run(
    prompt: str,
    /,
    *,
    model: str | None = None,
    provider: str | None = None,
    effort: str | None = None,
    speed: str | None = None,
    session: str | None = None,
    account: str | None = None,
    timeout: float = DEFAULT_AGENT_TIMEOUT,
) -> str:
    """Run a browser-use agent session: an LLM drives the real Aside Browser.

    Slow and token-spending (navigation plus model turns). Prefer `repl`/`read`
    when the steps are already known; use this when the task needs judgment,
    login flows, or reading a page the way a person would.

    The agent sees the browser, not this conversation - write self-contained
    prompts naming the site, the goal, and the exact output you want back.

    Args:
        prompt: the task, or a bare URL to just open that page.
        model: model id, or "provider/model" such as "anthropic/claude-opus-4-6"
            or a custom provider from `models.json`.
        provider: provider id when not folded into `model`.
        effort: off | minimal | low | medium | high | xhigh | max | ultrabrowse.
        speed: "default" or "fast".
        session: continue an existing session id.
        account: Aside account id, e.g. "u1".
        timeout: hard bound in seconds; raises AsideTimeout past it.
    """
    args = ["exec"]
    if session:
        args += ["--session", session]
    if account:
        args += ["--account", account]
    if model:
        args += ["-m", model]
    if provider:
        args += ["-p", provider]
    if speed:
        args += ["-s", speed]
    if effort:
        args += ["--effort", effort]
    args.append(prompt)
    stdout, stderr, code, _ = await asyncio.to_thread(_invoke, args, timeout, "aside agent session")
    if code != 0:
        raise AsideError(f"aside exited {code}: {(stderr or stdout).strip()[:1000]}")
    return stdout.strip()


agent = run


async def tabs(*, account: str | None = None) -> list[dict]:
    """List the browser's open tabs as dicts (id, targetId, url, title, active)."""
    return await repl(
        "console.log(JSON.stringify(await listBrowserTabs()))",
        parse=True, account=account, timeout=60,
    )


async def read(
    url: str | None = None,
    *,
    selector: str | None = None,
    limit: int = 20000,
    keep_open: bool = False,
    timeout: float = DEFAULT_REPL_TIMEOUT,
) -> str:
    """Extract visible text from a page - the cheap alternative to an agent session.

    Args:
        url: page to open; None reads the browser's currently active tab.
        selector: optional CSS selector to scope extraction.
        limit: max characters returned.
        keep_open: leave a newly opened tab open (ignored when reading the active tab).
        timeout: hard bound in seconds.
    """
    u, sel = _json.dumps(url), _json.dumps(selector)
    js = f"""
const url = {u}, sel = {sel};
const p = url ? await openTab(url) : await attachActiveBrowserTab();
if (url) await p.waitForLoadState('domcontentloaded');
const text = await p.evaluate((s) => {{
  const el = s ? document.querySelector(s) : document.body;
  return el ? el.innerText : '';
}}, sel);
const title = await p.title(), href = await p.url();
if (url && !{str(bool(keep_open)).lower()}) await p.close();
console.log(JSON.stringify({{ title, url: href, text: text.slice(0, {int(limit)}) }}));
"""
    data = await repl(js, parse=True, timeout=timeout)
    return f"# {data['title']}\n<{data['url']}>\n\n{data['text']}"


async def screenshot(
    path: str,
    *,
    url: str | None = None,
    full_page: bool = False,
    keep_open: bool = False,
    timeout: float = DEFAULT_REPL_TIMEOUT,
) -> str:
    """Save a PNG of a page to a real local path and return that path.

    The REPL's own filesystem is sandboxed - passing `path` to page.screenshot()
    inside JS silently redirects the file into the Aside session dir. This ships
    the bytes back over base64 and writes them from Python, so `path` is honoured.

    Pair with the attach_image skill to actually look at the result:
        p = await aside.screenshot("/tmp/page.png", url="https://example.com")
        await attach_image(p)

    Args:
        path: destination PNG path (parent dirs are created).
        url: page to open; None captures the currently active tab.
        full_page: capture the entire scrollable page.
        keep_open: leave a newly opened tab open.
        timeout: hard bound in seconds.
    """
    u = _json.dumps(url)
    js = f"""
const url = {u};
const p = url ? await openTab(url) : await attachActiveBrowserTab();
if (url) await p.waitForLoadState('domcontentloaded');
const buf = await p.screenshot({{ fullPage: {str(bool(full_page)).lower()} }});
if (url && !{str(bool(keep_open)).lower()}) await p.close();
console.log("B64:" + buf.toString('base64'));
"""
    body = await repl(js, timeout=timeout)
    line = next((l for l in body.splitlines() if l.startswith("B64:")), None)
    if not line:
        raise AsideError(f"no screenshot data returned:\n{body[:500]}")
    dest = Path(path).expanduser()
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(base64.b64decode(line[4:]))
    return str(dest)


def _read_providers(account: str | None = None) -> dict:
    cfg = config_dir(account) / "models.json"
    if not cfg.exists():
        return {}
    try:
        return _json.loads(cfg.read_text()).get("providers") or {}
    except ValueError:
        return {}


def providers(*, account: str | None = None) -> dict[str, str]:
    """Custom providers declared in the account's `models.json`, as {provider_id: baseUrl}."""
    return {pid: (prov.get("baseUrl") or "") for pid, prov in _read_providers(account).items()}


def models(*, account: str | None = None) -> list[str]:
    """List custom `provider/model` ids configured in the account's `models.json`.

    Returns [] when no custom providers are configured - the built-in providers
    picked in Aside's settings still work, they just are not listed here.
    """
    return [
        f"{pid}/{m.get('id')}"
        for pid, prov in _read_providers(account).items()
        for m in (prov.get("models") or [])
    ]


async def health(*, account: str | None = None, timeout: float = 15.0) -> dict:
    """Diagnose the whole chain before blaming a task: CLI, app, custom providers.

    Run this first whenever an aside call fails. Returns a dict with an `ok` flag
    and a human-readable `summary`. Local providers (a gateway on loopback declared
    in `models.json`) are probed only when they are actually configured, so a plain
    install with no custom providers still reports healthy.
    """
    report: dict[str, Any] = {}
    try:
        report["cli"] = _bin()
    except AsideNotRunning as exc:
        return {"ok": False, "cli": None, "summary": str(exc)}

    report["app_running"] = await asyncio.to_thread(_app_running)
    report["account"] = account or "u0"
    report["models"] = models(account=account)

    def _probe(base: str) -> str:
        try:
            with urllib.request.urlopen(f"{base.rstrip('/')}/health", timeout=timeout) as r:
                return r.read().decode()[:200]
        except Exception as exc:  # noqa: BLE001
            return f"unreachable: {exc}"

    local = {pid: url for pid, url in providers(account=account).items()
             if any(h in url for h in _LOOPBACK)}
    report["local_providers"] = {}
    for pid, url in local.items():
        body = await asyncio.to_thread(_probe, url)
        report["local_providers"][pid] = {"baseUrl": url, "response": body, "ok": '"ok"' in body}

    report["ok"] = bool(report["app_running"])

    problems = []
    if not report["app_running"]:
        problems.append("Aside Browser is not running - open the Aside app")
    for pid, info in report["local_providers"].items():
        if not info["ok"]:
            problems.append(
                f"local provider '{pid}' at {info['baseUrl']} looks down ({info['response']}); "
                "start it or pick a different model. Other providers still work."
            )
    report["summary"] = "; ".join(problems) if problems else (
        f"ok - app running, {len(report['models'])} custom model(s), "
        f"{len(report['local_providers'])} local provider(s)"
    )
    return report
