# pingpong's landing page

One page, built with [Astro](https://astro.build): real screenshots of the
apps, and a real stream recorded in the Linux container.

Builds target `https://ping-pong.sh` by default, including the canonical
URL, link previews, software structured data, `robots.txt` and
`sitemap.xml`. Set `SITE_URL` and `BASE_PATH` when building for another
domain or a project path.

```sh
npm install
npm run dev       # http://localhost:4321
npm run build     # astro check, then site/dist/
```

How it is put together and how its screenshots and video are made (`scripts/`):
[docs/development.md](../docs/development.md#the-website).
