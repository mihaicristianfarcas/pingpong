//! This site has one indexable page. Section anchors are parts of that page,
//! and build dates do not say when its content changed, so neither is listed.
import type { APIRoute } from 'astro';

export const GET: APIRoute = ({ site }) => {
  const home = new URL(import.meta.env.BASE_URL.replace(/\/?$/, '/'), site);
  const location = home.href.replace(/&/g, '&amp;');
  return new Response(
    `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n  <url><loc>${location}</loc></url>\n</urlset>\n`,
    { headers: { 'Content-Type': 'application/xml; charset=utf-8' } },
  );
};
