# Aside REPL — full API

Everything below was introspected from a **live** `aside repl` session
(`aside --version` 1.26.810.1915), not from docs. Regenerate with:

```python
await aside.repl("console.log(JSON.stringify(Object.keys(globalThis).sort()))", parse=True)
```

## Execution contract

- Code is passed as a single argv element by `aside.repl()` → multi-line JS with any
  quotes/backticks needs no escaping. Top-level `await` is supported.
- **Only `console.log(...)` is echoed.** Bare trailing expressions print nothing.
- The CLI **always exits 0**; success/failure is the `[ok | Nms]` / `[error | Nms]`
  stdout footer. `aside.repl()` parses it and raises `AsideReplError`.
- `openTab` and friends emit `✔︎ …` status lines and `[WARN] …` notices into stdout —
  stripped by `aside.repl()`. Print JSON on its own line and use `parse=True`.
- **The filesystem is sandboxed.** Writes to `/tmp` are silently redirected into
  `~/.aside/u/0/sessions/<session>/tmp/`. Return bytes as base64 and write them in
  Python (what `aside.screenshot()` does) rather than trusting a `path:` option.

## Globals — browsing core

- **`openTab(url)`** — open a new tab, make it active, return the page handle
- **`attachActiveBrowserTab()`** — handle for the tab the user is looking at (no navigation)
- **`attachBrowserTab(tab)`** / **`getTabByTargetId(id)`** — attach to a specific existing tab
- **`listBrowserTabs()`** — `[{id, targetId, url, title, active, windowId, faviconUrl}]`
- **`closeTab(tab)`** — close a tab
- **`snapshot(page)`** — accessibility tree `{tree, refs}` with `[ref=eN]` handles
- **`annotatedScreenshot(page)`** — screenshot with interactive elements labelled
- **`installPageScript(...)`** — inject a persistent page script
- **`blockToMarkdown(...)` / `markdownToBlockSpecs(...)`** — Aside block ↔ markdown
- **`display(x)`** — render a value in the Aside UI
- **`sleep(ms)`**, **`fetch(url)`** — timing and HTTP from the browser context
- **`page`** — ambient current page, `null` until a tab is opened/attached
- **`tabs`** — Array of tab handles for the current session
- **`pwd`** — sandboxed working directory (a string)

`snapshot(page)` is usually a better extraction target than raw HTML: it yields a
compact labelled tree (`heading "Example Domain" [level=1]`, `link "Learn more" [ref=e1]`).

## Page / tab objects (Playwright-shaped)

Navigation & lifecycle: `goto`, `goBack`, `goForward`, `reload`, `waitForLoadState`, `waitForURL`, `waitForSelector`, `waitForEvent`, `close`, `dispose`, `reconnect`, `bringToFront`

Query & interact: `locator`, `$`, `$$`, `$$eval`, `getByRole`, `getByLabel`, `getByText`, `frameLocator`, `frames`, `mainFrame`, `click`, `fill`, `evaluate`, `evaluateInFrame`

Capture & state: `screenshot`, `pdf`, `snapshot`, `content`, `title`, `url`, `video`, `viewportSize`, `refreshViewportSize`, `setCachedViewportSize`

Events & misc: `on`, `off`, `openPopupUrl`, `grantNotificationPermission`, `prepareForPotentialFileChooser`, `ensureAccess`, `resolveSessionId`

Properties: `browser`, `cdp`, `console`, `events`, `frameManager`, `keyboard`, `modifierState`, `mouse`, `targetId` — `keyboard`/`mouse` for raw input,
`cdp` for Chrome DevTools Protocol, `console` for captured page logs.

## Low-level input, media, filesystem

- **`cua`** — `click`, `doubleClick`, `drag`, `getVisibleScreenshot`, `keypress`, `move`, `scroll`, `type`
- **`captcha`** — `click`, `drag`, `readText`
- **`imagegen`** — `generate`, `getAbortSignal`, `models`, `readFile`, `readReferenceImage`, `sessionDir`
- **`imageSearch`** — `search`
- **`fs`** — `access`, `copyFile`, `lstat`, `mkdir`, `readFile`, `readdir`, `rename`, `resolvePath`, `rm`, `stat`, `unlink`, `writeFile`
- **`path`** — `_makeLong`, `basename`, `delimiter`, `dirname`, `extname`, `format`, `isAbsolute`, `join`, `matchesGlob`, `normalize`, `parse`, `posix`, `relative`, `resolve`, `sep`, `toNamespacedPath`, `win32`
- **`chrome`** — `(no enumerable methods; Chrome-profile bridge)`

`cua` is coordinate-based computer-use input (no selectors) — the fallback when a
canvas or PDF viewer exposes no DOM handles.

### The `fs` sandbox — four verified rules

1. `pwd` is the **session dir** (`~/.aside/u/0/sessions/<id>`), and **every `repl()`
   invocation gets a brand-new one**. A file written in one call raises `ENOENT` in the
   next. Do all file work inside a single call.
2. Host paths outside the session/project roots are rejected:
   `Error: Path escapes Project and session roots: /tmp/x.pdf`. This applies to
   `aside.pdf.*` too, so you cannot point it at `~/Downloads/foo.pdf`.
3. `~` is **not** expanded — `fs.resolvePath('~/Downloads/x')` yields a literal
   `<session>/~/Downloads/x` subdirectory. Never pass `~` paths.
4. `fs.readFile` returns a **Buffer**, not a string — call `.toString()`
   (or `.toString('base64')` to ship bytes back to Python).

To get a host file *in*, base64 it from Python into the JS source; to get bytes *out*,
`console.log("B64:" + buf.toString('base64'))` and decode in Python.

Size limit: the JS is passed as one argv element, and `ARG_MAX` is 1 MiB on this
machine, so an inlined base64 payload caps out around ~700 KB of binary. Above that,
have the page itself fetch the bytes (`fetch` is available in the REPL) instead of
inlining them.

## Integrations (authenticated via the browser's own sessions)

- **`gmail`** — `downloadAttachment`, `getInbox`, `getThread`, `openComposer`, `openReplyComposer`, `openThreadDetailsPage`, `search`
- **`slack`** — `getClient`, `invalidateCache`, `listWorkspaces`
- **`notion`** — `getClient`, `invalidateCache`, `listAccounts`
- **`linkedin`** — `acceptInvitation`, `getCompany`, `getConversation`, `getInbox`, `getJob`, `getMe`, `getProfile`, `getReceivedInvitations`, `getUserPosts`, `ignoreInvitation`, `invalidateCache`, `searchCompanies`, `searchPeople`, `sendInvitation`, `sendMessage`, `subscribeMessages`, `withdrawInvitation`
- **`twitter`** — `block`, `bookmark`, `deleteTweet`, `follow`, `getBookmarks`, `getDmConversation`, `getDmInbox`, `getMe`, `getNotifications`, `getTimeline`, `getTweet`, `getTweetThread`, `getUser`, `getUserTweets`, `like`, `mute`, `reply`, `retweet`, `search`, `sendDm`, `tweet`, `unblock`, `unbookmark`, `unfollow`, `unlike`, `unmute`, `unretweet`
- **`youtube`** — `getComments`, `getMetadata`, `getTranscript`, `listTranscriptLanguages`, `search`
- **`googleSearch`** — `search`
- **`googleDocs`** — `addComment`, `applyDiffs`, `applyDiffsAsSuggestions`, `connect`, `deleteSelection`, `dispose`, `getDocumentHTML`, `getDocumentText`, `getLiveText`, `getSelectedContent`, `getTitle`, `insertHtmlContent`, `insertText`, `page`, `parseUrl`, `pasteFromMarkdown`, `selectAll`, `selectTextRange`
- **`googleSheets`** — `addComment`, `connect`, `dispose`, `getSpreadsheetInfo`, `navigateToCell`, `page`, `readAllSheets`, `readSelection`, `readSheet`, `readSheetRich`, `setNote`, `switchSheet`, `writeHtml`, `writeMatrix`, `writeTsv`
- **`googlePeople`**
- **`googleAccounts`** — `list`, `print`
- **`applePasswords`** — `autofillLogin`, `capabilities`, `extensionBridgeRoute`, `getOtps`, `getPasswords`, `incognito`, `listLogins`, `requestAuth`, `saveLogin`, `verifyAuth`

These reuse the logged-in browser profile, so `gmail.search(...)` or
`twitter.getDmInbox()` work without API keys — far cheaper and more reliable than
asking an agent session to click through the UI.

## `aside` host object

- **`aside.channels`** — `create`, `list`, `restart`, `update`
- **`aside.pdf`** — `extractText`, `fillFormFields`, `getFormFields`, `merge`, `renderPages`, `rotate`, `split`
- **`aside.projects`** — `list`
- **`aside.routines`** — `get`, `list`
- **`aside.sessions`** — `archive`, `childSessions`, `current`, `get`, `list`, `markRead`, `markUnread`, `messageRows`, `messages`, `unarchive`, `update`
- **`aside.settings`** — `get`, `getAll`, `set`

`aside.pdf` is a full PDF toolkit (extract text, read/fill form fields, merge, split,
rotate, render pages) — no browser tab required. Every method takes a single **options
object** (`{filePath}`, `{filePath, outputPath}`, `{files, outputPath}`, …), never a
positional path, and each path is resolved through the sandbox. `aside.settings.get/set` reads and
writes user settings; `aside.sessions` enumerates past agent sessions.

## Runtime builtins

`Buffer`, `TextEncoder`, `TextDecoder`, `atob`, `btoa`, `fetch`, `console`,
`setTimeout`, `clearTimeout`, `setInterval`, `clearInterval`.

`Buffer` is the bridge out of the sandbox: `buf.toString('base64')` → decode in Python.

## Recipes

```python
# Structured scrape
rows = await aside.repl("""
const p = await openTab('https://news.ycombinator.com');
await p.waitForLoadState('domcontentloaded');
const out = await p.$$eval('.titleline > a', els =>
  els.slice(0, 10).map(e => ({ title: e.innerText, href: e.href })));
await p.close();
console.log(JSON.stringify(out));
""", parse=True)

# Accessibility tree of what the user is currently viewing
tree = await aside.repl("const p = await attachActiveBrowserTab(); "
                        "console.log((await snapshot(p)).tree)")

# Fill and submit a form
await aside.repl("""
const p = await openTab('https://example.com/login');
await p.fill('#email', 'me@example.com');
await p.getByRole('button', { name: 'Sign in' }).click();
await p.waitForURL(/dashboard/);
console.log(await p.title());
""")

# PDF text. Note: one call does write + extract, because the sandbox is per-call,
# and the host file is carried in as base64 since /Users/... paths are rejected.
import base64, json
raw = base64.b64encode(open("/path/doc.pdf", "rb").read()).decode()
out = await aside.repl(f"""
await fs.writeFile('in.pdf', Buffer.from({json.dumps(raw)}, 'base64'));
const r = await aside.pdf.extractText({{ filePath: 'in.pdf' }});
console.log(JSON.stringify(r));
""", parse=True, timeout=180)
# -> {"pageCount": 1, "pages": [{"page": 1, "text": "..."}]}
```
