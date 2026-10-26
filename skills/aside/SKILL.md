---
name: aside
description: Drive the Aside Browser from the Prime Agent kernel - run deterministic Playwright-style browser automation, extract page text, capture screenshots of live pages you can then look at, and run browser-use agent sessions in a real logged-in browser. Use when the user says "aside", or asks to browse, scrape, or automate a website, log into a site, fill a form, test a web flow, read a page behind a login, or screenshot a real page.
---

# aside

Python-backed skill wrapping the `aside` CLI, which drives the **Aside Browser**
(a real Chromium app that must be running). Call it from the kernel:

```python
h = await aside.health()                                  # run this first when anything fails
t = await aside.tabs()                                    # [{title, url, active, targetId}, ...]
md = await aside.read("https://example.com")              # visible text, no LLM
p  = await aside.screenshot("/tmp/p.png", url="https://x.com")
out = await aside.repl("const p = await attachActiveBrowserTab();"      # any JS, no LLM
                       "console.log(await p.title())")
ans = await aside("Log into the admin panel and report the latest order")  # LLM agent session
```

## Choose the cheap path first

| Need | Use | Cost |
|---|---|---|
| Known steps: navigate, click, fill, extract | `aside.repl(js)` | free, fast, reproducible |
| Just the page's text | `aside.read(url)` | free |
| See what a page looks like | `aside.screenshot(path, url=...)` + `attach_image` | free |
| Needs judgment, login flows, unknown layout | `await aside(prompt, ...)` | tokens + browser turns, slow |

`repl` is deterministic automation with **no LLM**. Reach for an agent session only
when the task genuinely needs judgment — not to click a button whose selector you know.

To actually *see* a page, screenshot then attach:

```python
p = await aside.screenshot("/tmp/page.png", url="https://example.com", full_page=True)
await attach_image(p)
```

## Three footguns this wrapper absorbs

1. **`aside repl` exits 0 even when the JS throws.** The only signal is an
   `[error | Nms]` footer in stdout. `aside.repl()` raises `AsideReplError`.
   Never judge a raw `aside repl` bash call by its exit code.
2. **Output carries ANSI escapes even under `NO_COLOR`**, plus `openTab` status
   lines and `[WARN]` noise. All stripped here; `parse=True` returns parsed JSON.
3. **The REPL filesystem is sandboxed *and* per-call.** `page.screenshot({path:
   "/tmp/x.png"})` silently redirects into `~/.aside/u/0/sessions/<id>/tmp/`; host
   paths outside the session/project roots are rejected outright; and **every
   `repl()` call gets a fresh session dir**, so a file written in one call is gone
   in the next. Keep file work inside a single call and return bytes as base64 —
   which is what `aside.screenshot()` does, so the path you pass is the path you get.

Every call is bounded (`repl` 120s, agent 900s) and raises `AsideTimeout` naming
what was awaited — never wrap these in a guessed sleep.

## Writing REPL JavaScript

Code is passed as one argv element, so multi-line JS with any mix of `'`, `"`, and
backticks needs **no escaping**. Top-level `await` works. Only `console.log(...)`
is echoed — bare expressions are not. Start every script by getting a page handle:
the ambient `page` global is **`null`** until you call `openTab(url)` (new tab) or
`attachActiveBrowserTab()` (the tab the user is on).

```python
data = await aside.repl("""
const p = await openTab('https://news.ycombinator.com');
await p.waitForLoadState('domcontentloaded');
const rows = await p.$$eval('.titleline > a', els =>
  els.slice(0, 5).map(e => ({ title: e.innerText, href: e.href })));
await p.close();
console.log(JSON.stringify(rows));
""", parse=True)
```

Page objects are Playwright-shaped (`goto`, `click`, `fill`, `getByRole`,
`waitForSelector`, `$$eval`, `evaluate`, `screenshot`, `pdf`, `keyboard`, `mouse`).
Globals go far beyond browsing — `snapshot()` a11y trees, `cua` raw input,
`gmail`, `slack`, `notion`, `twitter`, `youtube`, `googleSheets`, `aside.pdf`,
`aside.sessions`. Full verified surface: [references/repl-api.md](references/repl-api.md).

## Agent sessions

```python
ans = await aside("Find the pricing page and list every tier with its price",
                  effort="high", timeout=900)   # model= optional; default from settings
```

- `model` accepts `provider/model`; also `provider=`, `effort=`, `speed=`,
  `session=` (continue a session), `account=`.
- `effort`: `off | minimal | low | medium | high | xhigh | max | ultrabrowse`
  (`ultrabrowse` = proactive mode, highest thinking).
- The agent sees the browser, **not this conversation** — write self-contained
  prompts naming the site, the goal, and the exact output wanted.
- **Returns the whole session transcript** (`Thinking:` lines, tool calls, then the
  final answer last), not just an answer. When you need to parse the result, ask
  for a sentinel: `"...end your reply with ANSWER: <value>"`.

### Custom providers (incl. a local gateway)

Omitting `model` uses the user's default from Aside's settings, which is usually
what you want. Beyond the providers in the settings UI, Aside also reads an
**undocumented, hot-reloaded** custom-provider file at
`~/.aside/u/<account>/models.json` — no app restart needed:

```json
{ "providers": { "my-provider": {
    "name": "My Provider", "baseUrl": "https://api.example.com",
    "apiKey": "<key, or a placeholder if the endpoint ignores it>",
    "api": "anthropic-messages",
    "models": [{ "id": "my-model", "reasoning": true,
                 "input": ["text", "image"], "contextWindow": 200000, "maxTokens": 32000 }] } } }
```

`api` accepts `anthropic-messages`, `openai-completions`, `openai-responses`, and
friends; the SDK appends the route (`/v1/messages`), so `baseUrl` is the origin only.
Then use `model="my-provider/my-model"`.

Prefer `models.json` over `settings.json`/`credentials.json` for this: the daemon
**caches** those two, so edits there need a restart.

`aside.models()` lists configured `provider/model` ids and `aside.providers()` maps
provider ids to base URLs. If a provider's `baseUrl` points at this machine (loopback),
`health()` probes its `/health` endpoint — but only when such a provider is actually
configured, so a plain install still reports healthy. All three read the default
account (`~/.aside/u/0`); pass `account="u1"` to read the account your browser
calls target instead.

## Troubleshooting

`await aside.health()` first — it checks the CLI binary, whether Aside.app is
running, gateway `/health`, and configured models, and returns a `summary`.

- `AsideNotRunning` → open `/Applications/Aside.app` (`pgrep -fl Aside.app`).
- A local provider looks down → curl its `baseUrl` `/health`, or drop `model=` to
  fall back to the default provider. Other providers keep working.
- `aside account list | status | use <id>`; per-call `account="u1"`.
- CLI update: `aside --update`. Docs: https://docs.aside.com/help/developers.md
- MCP mode for other harnesses: `{"mcpServers": {"aside": {"command": "aside", "args": ["mcp"]}}}`
