# rms wiki sources

These Markdown files are the user guide of rms, written to work in two places:

- **in the repository** — links between pages use `Page.md`, images use `../screenshots/<file>.png` and links to
  repository files use relative paths (`../API.md`, `../../config.example.toml`), so everything resolves when
  browsing `docs/wiki/` on GitHub or in an editor;
- **in the GitHub wiki** — after the small path rewrite below.

Start at [Home.md](Home.md); `_Sidebar.md` is the wiki sidebar. This `README.md` is not a wiki page.

## Publishing to the GitHub wiki

The wiki is a separate Git repository, `<repo>.wiki.git`, created when the first page is saved from the
*Wiki* tab of the GitHub project (create any page once, then replace it).

```bash
OWNER=<github-user-or-org>; REPO=rust-media-server
git clone "https://github.com/$OWNER/$REPO.wiki.git" /tmp/rms-wiki

# pages (without this README) and screenshots
cp $(ls docs/wiki/*.md | grep -v '/README.md$') /tmp/rms-wiki/
mkdir -p /tmp/rms-wiki/images && cp docs/screenshots/*.png /tmp/rms-wiki/images/

cd /tmp/rms-wiki
sed -i -E \
  -e 's#\(\.\./screenshots/#(images/#g' \
  -e "s#\]\(\.\./\.\./#](https://github.com/$OWNER/$REPO/blob/main/#g" \
  -e "s#\]\(\.\./#](https://github.com/$OWNER/$REPO/blob/main/docs/#g" \
  -e 's#\]\(([A-Za-z0-9-]+)\.md(\#[^)]*)?\)#](\1\2)#g' \
  *.md
git add -A && git commit -m "Update the rms wiki" && git push
```

What the rewrite does, in order:

1. `../screenshots/x.png` → `images/x.png` (the copies pushed with the wiki). To avoid duplicating the images,
   use `https://raw.githubusercontent.com/$OWNER/$REPO/main/docs/screenshots/` as the replacement instead and
   skip the `images/` copy: the wiki then always shows the screenshots of `main`.
2. `../../file` (repository root: README, `config.example.toml`, `deploy/…`) → `https://github.com/…/blob/main/file`.
3. `../file` (other `docs/` documents such as `API.md`) → `https://github.com/…/blob/main/docs/file`.
4. `Page.md` and `Page.md#anchor` → `Page` and `Page#anchor` (GitHub wiki page links).

Page names come from the file names (`Users-API-Keys-and-Buckets.md` → *Users API Keys and Buckets*). Keep the
files here as the source of truth and re-run the commands after each change; edits made in the wiki web editor
would be overwritten.

## Checking links

Before publishing, check that every relative link, anchor and image of `README.md`, `README.it.md` and these
pages resolves in the repository, for example with [lychee](https://github.com/lycheeverse/lychee):
`lychee --offline --include-fragments README.md README.it.md docs/wiki/*.md`.
