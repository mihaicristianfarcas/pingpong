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
npm test          # build and DOM keyboard regressions
```

Tab and Shift+Tab visit clickable links, buttons and FAQ toggles in page
order, from the header to the footer. Every gallery choice and download
option has its own stop. Enter activates links; Enter or Space activates
buttons and FAQ toggles. The mobile menu also accepts arrow keys, Home
and End; Escape closes it and returns focus to Menu. The first Tab reveals
Skip to content, which moves focus to the first action in the main content.

The header hides as you scroll down and returns as you scroll up or reach
the top. Refreshing or returning to the page resets it to visible, including
when your scroll position is restored. It stays visible while its controls
have focus or the mobile menu is open. Reduced motion removes its slide
transition.

On refresh, the saved scroll position is restored after the component
scripts finish arranging the gallery and downloads and the fonts settle.
The font wait has a 600 ms fallback after component initialization.

Copy results and changed download selections are announced to screen
readers. The stream video has native controls without JavaScript and
respects reduced motion, including changes made while the page is open.

Without JavaScript, all screenshots and download panels are shown. Copy
and selection controls appear when their handlers are ready.

The keyboard tests cover control order, focus changes and hidden content
in a simulated DOM at desktop and phone widths. Browser rendering and
screen-reader checks are separate.

How it is put together and how its screenshots and video are made (`scripts/`):
[docs/development.md](../docs/development.md#the-website).
